//! GitHub data access: token discovery, a failure-tolerant REST client, org
//! expansion, and the bounded-concurrency fan-out that inspects repositories.
//!
//! Everything network-facing is reachable through a small surface that takes an
//! **injectable base URL**, so tests point the client at a mock HTTP server
//! (see the `tests` module's `wiremock` usage) instead of `api.github.com` — no
//! test ever needs real credentials or real network access.
//!
//! The pieces, smallest to largest:
//!
//! 1. [`discover_token`] — token discovery (`ACTSTAT_GITHUB_TOKEN` → `GH_TOKEN`
//!    → `GITHUB_TOKEN` → `gh auth token` → unauthenticated-with-warning).
//! 2. [`GitHubClient`] — a `reqwest`-backed client with retry/backoff on
//!    transient failures and explicit mapping of `401`/`403`/`404`/rate-limit
//!    statuses to typed [`GitHubError`]s.
//! 3. [`GitHubClient::list_org_repos`] — expand one org into its repositories,
//!    following pagination to exhaustion and honoring configured filters.
//! 4. [`resolve_repositories`] — expand every configured org (bounded
//!    concurrency), then merge with the explicit repos through
//!    [`resolve_repos`] into one de-duplicated, alphabetically-stable list.
//! 5. [`GitHubClient::list_run_jobs`] — fetch failed jobs and steps for a
//!    non-passing workflow run.
//! 6. [`fetch_repo_reports`] — run a per-repository async operation across many
//!    repos with bounded concurrency, isolating each repo's failure into a
//!    [`RepoReport`] error record so one bad repo never aborts the whole run.
//! 7. [`collect_repo_reports`] — the Phase 4 pipeline: fan out over the resolved
//!    repos, selecting each one's most recent settled default-branch commits and
//!    enriching their failed runs into a sorted list of [`RepoReport`]s ready for
//!    any renderer.

use std::future::Future;
use std::time::Duration;

use futures::stream::StreamExt as _;
use reqwest::header::HeaderMap;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use thiserror::Error;

use crate::config::{resolve_repos, Config, OrgSource, RepoName};
use crate::model::{
    CommitReport, Conclusion, JobReport, RepoReport, RunReport, StepReport,
};

/// The real GitHub REST API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// `User-Agent` sent with every request (GitHub requires one).
const USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Environment variables consulted for a token, in precedence order.
const TOKEN_ENV_VARS: [&str; 3] =
    ["ACTSTAT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

// --- Token discovery -------------------------------------------------------

/// Where an auth token came from (purely for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// A named environment variable (e.g. `GH_TOKEN`).
    Env(String),
    /// The `gh auth token` command.
    GhCli,
    /// No token found; requests will be unauthenticated.
    Unauthenticated,
}

/// The result of token discovery: the token (if any) and where it came from.
#[derive(Debug, Clone)]
pub struct DiscoveredToken {
    /// The bearer token, or `None` when running unauthenticated.
    pub token: Option<String>,
    /// Where the token was found.
    pub source: TokenSource,
}

impl DiscoveredToken {
    /// Whether a usable token was found.
    pub fn is_authenticated(&self) -> bool {
        self.token.is_some()
    }

    /// A one-line stderr warning to emit when running unauthenticated, or
    /// `None` when a token was found. Diagnostics belong on stderr so machine
    /// output on stdout stays pipe-clean.
    pub fn unauthenticated_warning(&self) -> Option<&'static str> {
        if self.token.is_none() {
            Some(
                "no GitHub token found (set ACTSTAT_GITHUB_TOKEN, GH_TOKEN, or \
                 GITHUB_TOKEN, or run `gh auth login`); making unauthenticated \
                 requests — private repos and org expansion are limited and \
                 rate limits are low",
            )
        } else {
            None
        }
    }
}

/// Discover a GitHub token using the documented precedence:
///
/// 1. `ACTSTAT_GITHUB_TOKEN`
/// 2. `GH_TOKEN`, then `GITHUB_TOKEN`
/// 3. `gh auth token` (if `gh` is installed and authenticated)
/// 4. otherwise none — the caller should warn and proceed unauthenticated.
///
/// Empty/whitespace-only values are treated as absent.
pub fn discover_token() -> DiscoveredToken {
    discover_token_with(
        |key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()),
        gh_auth_token,
    )
}

/// Pure core of [`discover_token`] with the environment and `gh` lookups
/// injected, so every branch is testable without touching the process
/// environment or shelling out.
fn discover_token_with(
    env: impl Fn(&str) -> Option<String>,
    gh: impl FnOnce() -> Option<String>,
) -> DiscoveredToken {
    for key in TOKEN_ENV_VARS {
        if let Some(value) = env(key).map(|v| v.trim().to_string()) {
            if !value.is_empty() {
                return DiscoveredToken {
                    token: Some(value),
                    source: TokenSource::Env(key.to_string()),
                };
            }
        }
    }
    if let Some(value) = gh().map(|v| v.trim().to_string()) {
        if !value.is_empty() {
            return DiscoveredToken {
                token: Some(value),
                source: TokenSource::GhCli,
            };
        }
    }
    DiscoveredToken {
        token: None,
        source: TokenSource::Unauthenticated,
    }
}

/// Shell out to `gh auth token`. Returns `None` if `gh` is missing, not
/// authenticated, or emits nothing.
fn gh_auth_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

// --- Errors ----------------------------------------------------------------

/// An error from a single GitHub API operation.
///
/// Variants map directly to the resilience requirements: auth failures, the
/// not-found case, rate/abuse limits, transient server/connection errors, and a
/// decode fallback. The [`std::fmt::Display`] rendering is concise enough to
/// store verbatim in a per-repository [`RepoReport::error`].
#[derive(Debug, Error)]
pub enum GitHubError {
    /// `401` — missing or invalid credentials.
    #[error("unauthorized (401): {0}")]
    Unauthorized(String),

    /// `403` — forbidden (e.g. token lacks the required scope or org access).
    #[error("forbidden (403): {0}")]
    Forbidden(String),

    /// A primary (`429`/`403` with `x-ratelimit-remaining: 0`) or secondary
    /// ("abuse") rate limit. Transient: retried before surfacing.
    #[error("rate limited: {message}")]
    RateLimited {
        /// The API's explanation.
        message: String,
        /// `Retry-After` seconds, when the API supplied one.
        retry_after: Option<u64>,
    },

    /// `404` — repository/org not found, or invisible to this token.
    #[error("not found (404): {0}")]
    NotFound(String),

    /// Any other non-success HTTP status. `5xx` is treated as transient.
    #[error("unexpected status {status}: {message}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The API's explanation (or a truncated body).
        message: String,
    },

    /// A transport/connection error (DNS, TLS, reset, timeout). Transient.
    #[error("request failed: {0}")]
    Transport(String),

    /// The response could not be deserialized into the expected shape.
    #[error("failed to parse response: {0}")]
    Decode(String),
}

// --- Retry policy ----------------------------------------------------------

/// Retry/backoff policy for transient failures (`5xx`, `429`, connection
/// errors). Tests use [`RetryConfig::no_delay`] so retries are exercised
/// without any wall-clock wait.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries *after* the initial attempt.
    pub max_retries: u32,
    /// Base backoff; the delay before retry `n` is `base_delay * 2^n`.
    pub base_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

impl RetryConfig {
    /// A policy with no backoff delay, for fast, deterministic tests.
    pub fn no_delay(max_retries: u32) -> Self {
        RetryConfig {
            max_retries,
            base_delay: Duration::ZERO,
        }
    }

    /// Exponential backoff for the given (zero-based) retry attempt.
    fn backoff(&self, attempt: u32) -> Duration {
        self.base_delay
            .saturating_mul(2u32.saturating_pow(attempt.min(16)))
    }
}

// --- Client ----------------------------------------------------------------

/// A failure-tolerant GitHub REST client.
///
/// The base URL is injectable so tests target a local mock server; in
/// production it is [`DEFAULT_BASE_URL`]. The client adds the standard GitHub
/// headers, optionally authenticates, retries transient failures with
/// exponential backoff, and maps non-success statuses to typed [`GitHubError`]s.
#[derive(Debug, Clone)]
pub struct GitHubClient {
    http: Client,
    /// Normalized base URL with no trailing slash.
    base_url: String,
    token: Option<String>,
    retry: RetryConfig,
}

impl GitHubClient {
    /// Build a client against an explicit base URL with a given retry policy.
    pub fn new(
        base_url: impl Into<String>,
        token: Option<String>,
        retry: RetryConfig,
    ) -> Result<Self, GitHubError> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| GitHubError::Transport(e.to_string()))?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(GitHubClient {
            http,
            base_url,
            token,
            retry,
        })
    }

    /// Build a client against the real GitHub API with the default retry policy.
    pub fn github(token: Option<String>) -> Result<Self, GitHubError> {
        Self::new(DEFAULT_BASE_URL, token, RetryConfig::default())
    }

    /// Fetch and deserialize a single JSON resource at `path` (no pagination).
    ///
    /// The building block Phase 4 composes run/job fetching from; `path` may be
    /// an API path (`/repos/...`) or an absolute URL.
    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, GitHubError> {
        Ok(self.get_page::<T>(path).await?.items)
    }

    /// Fetch every page of a paginated array endpoint, following `Link:
    /// rel="next"` to exhaustion and concatenating the results.
    pub async fn get_paginated<T: DeserializeOwned>(
        &self,
        first_path: &str,
    ) -> Result<Vec<T>, GitHubError> {
        let mut url = self.resolve_url(first_path);
        let mut items = Vec::new();
        loop {
            let page: Page<Vec<T>> = self.get_page(&url).await?;
            items.extend(page.items);
            match page.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(items)
    }

    /// Expand one org into its repositories.
    ///
    /// Follows pagination to exhaustion and applies the org's filters:
    /// archived and forked repos are dropped unless explicitly included, and any
    /// repo in `exclude` is removed. The result is the org's `owner/name`
    /// entries in API order (the caller's [`resolve_repos`] handles final
    /// dedup/sort).
    pub async fn list_org_repos(
        &self,
        org: &OrgSource,
    ) -> Result<Vec<RepoName>, GitHubError> {
        let path = format!("/orgs/{}/repos?per_page=100", org.org);
        let raw: Vec<ApiRepo> = self.get_paginated(&path).await?;
        Ok(raw
            .into_iter()
            .filter(|r| org.include_archived || !r.archived)
            .filter(|r| org.include_forks || !r.fork)
            .map(|r| RepoName {
                owner: r.owner.login,
                name: r.name,
            })
            .filter(|name| {
                !org.exclude.iter().any(|excluded| {
                    repo_name_eq_ignore_ascii_case(excluded, name)
                })
            })
            .collect())
    }

    /// Fetch the failed jobs (and their failed steps) for one run.
    ///
    /// Issues `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs` and keeps
    /// only *non-successful* jobs, each carrying only its actionable failed
    /// steps; successful/skipped jobs and passing steps are dropped so the
    /// report shows just what is worth investigating. Only call this for a run
    /// whose conclusion is non-successful.
    pub async fn list_run_jobs(
        &self,
        repo: &RepoName,
        run_id: u64,
    ) -> Result<Vec<JobReport>, GitHubError> {
        let path = format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs?per_page=100",
            repo.owner, repo.name
        );
        let response: JobsResponse = self.get(&path).await?;
        Ok(response
            .jobs
            .into_iter()
            .filter_map(normalize_job)
            .collect())
    }

    /// Fetch + normalize one repository's most recent settled default-branch
    /// commits and enrich every problem run with its failed jobs/steps. The
    /// per-repo unit of work behind [`collect_repo_reports`].
    async fn collect_commits(
        &self,
        repo: &RepoName,
        limit: u32,
    ) -> Result<Vec<CommitReport>, GitHubError> {
        let default_branch = self.repo_default_branch(repo).await?;
        let api_runs = self
            .fetch_default_branch_runs(repo, &default_branch)
            .await?;
        let selected = select_settled_commits(api_runs, limit);
        let mut commits = Vec::with_capacity(selected.len());
        for commit in selected {
            commits.push(
                self.build_commit_report(repo, &default_branch, commit)
                    .await?,
            );
        }
        Ok(commits)
    }

    /// Resolve a repository's default branch.
    async fn repo_default_branch(
        &self,
        repo: &RepoName,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}", repo.owner, repo.name);
        let detail: ApiRepoDetail = self.get(&path).await?;
        Ok(detail.default_branch)
    }

    /// Fetch one bounded window of recent workflow runs on the default branch.
    ///
    /// This intentionally does not use `status=completed`: non-completed runs
    /// are needed to identify and skip commits that have not fully settled yet.
    async fn fetch_default_branch_runs(
        &self,
        repo: &RepoName,
        branch: &str,
    ) -> Result<Vec<ApiRun>, GitHubError> {
        let branch = percent_encode_query_value(branch);
        let path = format!(
            "/repos/{}/{}/actions/runs?branch={branch}&per_page=100",
            repo.owner, repo.name
        );
        let response: RunsResponse = self.get(&path).await?;
        Ok(response.workflow_runs)
    }

    /// Build one commit report and enrich every problem run. Successful,
    /// skipped, and neutral runs need no job expansion.
    async fn build_commit_report(
        &self,
        repo: &RepoName,
        default_branch: &str,
        selected: SelectedCommit,
    ) -> Result<CommitReport, GitHubError> {
        let title = selected
            .runs
            .iter()
            .find_map(|r| r.display_title.clone())
            .unwrap_or_default();
        let event = selected
            .runs
            .iter()
            .find_map(|r| r.event.clone())
            .unwrap_or_default();
        let finished_at = latest_updated_at(&selected.runs);
        let duration_seconds = compute_commit_duration_seconds(&selected.runs);
        let short_sha: String = selected.full_sha.chars().take(7).collect();
        let url = format!(
            "https://github.com/{}/{}/commit/{}",
            repo.owner, repo.name, selected.full_sha
        );

        let mut runs = Vec::with_capacity(selected.runs.len());
        for api in selected.runs {
            let (run_id, mut run) = normalize_run(api);
            if run.is_problem() && run_id != 0 {
                run.jobs = self.list_run_jobs(repo, run_id).await?;
            }
            runs.push(run);
        }
        let conclusion = CommitReport::aggregate_conclusion(&runs);

        Ok(CommitReport {
            sha: short_sha,
            title,
            branch: default_branch.to_string(),
            event,
            conclusion,
            url,
            finished_at,
            duration_seconds,
            runs,
        })
    }

    /// Fetch a single page, retrying transient failures per [`RetryConfig`].
    async fn get_page<T: DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Page<T>, GitHubError> {
        let full = self.resolve_url(url);
        let mut attempt: u32 = 0;
        loop {
            match self.attempt_get::<T>(&full).await {
                Attempt::Done(page) => return Ok(page),
                Attempt::Fail(err) => return Err(err),
                Attempt::Retry(err) => {
                    if attempt >= self.retry.max_retries {
                        return Err(err);
                    }
                    let retry_after = match &err {
                        GitHubError::RateLimited { retry_after, .. } => {
                            *retry_after
                        }
                        _ => None,
                    };
                    let delay = self.retry_delay(attempt, retry_after);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// One HTTP attempt, classified into success / retryable / fatal.
    async fn attempt_get<T: DeserializeOwned>(&self, url: &str) -> Attempt<T> {
        let response = match self.request(url).send().await {
            Ok(resp) => resp,
            // Connection-level errors are transient.
            Err(e) => {
                return Attempt::Retry(GitHubError::Transport(e.to_string()))
            }
        };

        let status = response.status();
        if status.is_success() {
            let next = parse_next_link(response.headers());
            return match response.json::<T>().await {
                Ok(items) => Attempt::Done(Page { items, next }),
                Err(e) => Attempt::Fail(GitHubError::Decode(e.to_string())),
            };
        }

        // Non-success: read headers before consuming the body for the message.
        let code = status.as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        let message = error_message(&body);

        if is_rate_limited(status, &headers, &body) {
            return Attempt::Retry(GitHubError::RateLimited {
                message,
                retry_after: parse_retry_after(&headers),
            });
        }

        match status {
            StatusCode::UNAUTHORIZED => {
                Attempt::Fail(GitHubError::Unauthorized(message))
            }
            StatusCode::FORBIDDEN => {
                Attempt::Fail(GitHubError::Forbidden(message))
            }
            StatusCode::NOT_FOUND => {
                Attempt::Fail(GitHubError::NotFound(message))
            }
            s if s.is_server_error() => Attempt::Retry(GitHubError::Status {
                status: code,
                message,
            }),
            _ => Attempt::Fail(GitHubError::Status {
                status: code,
                message,
            }),
        }
    }

    /// The backoff before a given retry, honoring a `Retry-After` hint (capped)
    /// in production while staying instant when `base_delay` is zero (tests).
    fn retry_delay(&self, attempt: u32, retry_after: Option<u64>) -> Duration {
        if self.retry.base_delay.is_zero() {
            return Duration::ZERO;
        }
        let backoff = self.retry.backoff(attempt);
        match retry_after {
            Some(secs) => backoff.max(Duration::from_secs(secs.min(60))),
            None => backoff,
        }
    }

    /// Build a GET request with the standard GitHub headers and optional auth.
    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req
    }

    /// Resolve a path or absolute URL against the base URL. Absolute URLs (such
    /// as the `Link` header's next page) are used verbatim.
    fn resolve_url(&self, url: &str) -> String {
        if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("{}/{}", self.base_url, url.trim_start_matches('/'))
        }
    }
}

/// A single fetched page: the deserialized body plus the next-page URL, if any.
struct Page<T> {
    items: T,
    next: Option<String>,
}

/// The classification of one HTTP attempt.
enum Attempt<T> {
    /// Succeeded; here is the page.
    Done(Page<T>),
    /// Failed transiently; retry if attempts remain.
    Retry(GitHubError),
    /// Failed permanently; surface immediately.
    Fail(GitHubError),
}

/// Minimal projection of a repository from `GET /orgs/{org}/repos`.
#[derive(Debug, Deserialize)]
struct ApiRepo {
    name: String,
    owner: ApiOwner,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    fork: bool,
}

/// The `owner` object on a repository payload.
#[derive(Debug, Deserialize)]
struct ApiOwner {
    login: String,
}

/// Minimal projection of a repository from `GET /repos/{owner}/{repo}`.
#[derive(Debug, Deserialize)]
struct ApiRepoDetail {
    #[serde(default)]
    default_branch: String,
}

// --- Workflow run / job payloads & normalization ---------------------------

/// Envelope of `GET /repos/{owner}/{repo}/actions/runs`.
#[derive(Debug, Deserialize)]
struct RunsResponse {
    /// The runs; defaulted so a `{}`/partial body yields "no runs" rather than
    /// a decode error.
    #[serde(default)]
    workflow_runs: Vec<ApiRun>,
}

/// One workflow run from the runs listing.
///
/// Every field tolerates absence (`#[serde(default)]`) so a partial or
/// malformed payload degrades to sensible defaults instead of failing the whole
/// repository — only the fields actually used by [`normalize_run`] are modeled.
#[derive(Debug, Clone, Deserialize)]
struct ApiRun {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    workflow_id: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_title: Option<String>,
    #[serde(default)]
    run_number: u64,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    head_branch: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    run_started_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

/// Envelope of `GET /repos/{owner}/{repo}/actions/runs/{id}/jobs`.
#[derive(Debug, Deserialize)]
struct JobsResponse {
    #[serde(default)]
    jobs: Vec<ApiJob>,
}

/// One job within a run's jobs listing.
#[derive(Debug, Deserialize)]
struct ApiJob {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    steps: Vec<ApiStep>,
}

/// One step within a job.
#[derive(Debug, Deserialize)]
struct ApiStep {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    number: u64,
    #[serde(default)]
    conclusion: Option<String>,
}

/// Normalize one API run into a [`RunReport`], returning the run id alongside it
/// (the id is needed to fetch jobs for problem runs but is not part of the
/// rendered model). The head SHA is shortened to 7 characters.
fn normalize_run(api: ApiRun) -> (u64, RunReport) {
    let sha: String =
        api.head_sha.unwrap_or_default().chars().take(7).collect();
    let duration_seconds = compute_duration_seconds(
        api.run_started_at.as_deref(),
        api.created_at.as_deref(),
        api.updated_at.as_deref(),
    );
    let report = RunReport {
        workflow: api.name.unwrap_or_default(),
        title: api.display_title.unwrap_or_default(),
        run_number: api.run_number,
        event: api.event.unwrap_or_default(),
        branch: api.head_branch.unwrap_or_default(),
        sha,
        conclusion: Conclusion::from_github(api.conclusion.as_deref()),
        url: api.html_url.unwrap_or_default(),
        created_at: api.created_at.unwrap_or_default(),
        updated_at: api.updated_at.unwrap_or_default(),
        duration_seconds,
        jobs: vec![],
    };
    (api.id, report)
}

/// A selected commit with its full SHA and deduplicated latest workflow runs.
#[derive(Debug)]
struct SelectedCommit {
    full_sha: String,
    runs: Vec<ApiRun>,
}

/// Select the `limit` most recent settled commits from a newest-first run
/// listing. Runs are grouped by full head SHA in first-appearance order, then
/// deduplicated to the latest run per workflow before settlement is evaluated.
fn select_settled_commits(
    runs: Vec<ApiRun>,
    limit: u32,
) -> Vec<SelectedCommit> {
    let mut groups: Vec<(String, Vec<ApiRun>)> = Vec::new();
    for run in runs {
        let full_sha = run.head_sha.clone().unwrap_or_default();
        if let Some((_, grouped)) =
            groups.iter_mut().find(|(sha, _)| sha == &full_sha)
        {
            grouped.push(run);
        } else {
            groups.push((full_sha, vec![run]));
        }
    }

    let mut selected = Vec::new();
    for (full_sha, runs) in groups {
        let runs = dedupe_latest_runs_per_workflow(runs);
        if runs.iter().all(run_is_completed) {
            selected.push(SelectedCommit { full_sha, runs });
            if selected.len() >= limit as usize {
                break;
            }
        }
    }
    selected
}

/// Collapse repeated attempts of the same workflow to the highest run id.
fn dedupe_latest_runs_per_workflow(runs: Vec<ApiRun>) -> Vec<ApiRun> {
    let mut keys: Vec<String> = Vec::new();
    let mut deduped: Vec<ApiRun> = Vec::new();

    for run in runs {
        let key = workflow_key(&run);
        if let Some(index) = keys.iter().position(|k| k == &key) {
            if run.id > deduped[index].id {
                deduped[index] = run;
            }
        } else {
            keys.push(key);
            deduped.push(run);
        }
    }

    deduped
}

fn workflow_key(run: &ApiRun) -> String {
    match run.workflow_id {
        Some(id) => format!("id:{id}"),
        None => format!("name:{}", run.name.as_deref().unwrap_or_default()),
    }
}

fn run_is_completed(run: &ApiRun) -> bool {
    run.status.as_deref() == Some("completed")
}

fn latest_updated_at(runs: &[ApiRun]) -> String {
    let mut fallback: Option<&str> = None;
    let mut latest: Option<(&str, jiff::Timestamp)> = None;

    for run in runs {
        let Some(value) = run.updated_at.as_deref().filter(|v| !v.is_empty())
        else {
            continue;
        };
        fallback.get_or_insert(value);
        let Ok(timestamp) = value.parse::<jiff::Timestamp>() else {
            continue;
        };
        if latest.is_none_or(|(_, current)| timestamp > current) {
            latest = Some((value, timestamp));
        }
    }

    latest
        .map(|(value, _)| value.to_string())
        .or_else(|| fallback.map(str::to_string))
        .unwrap_or_default()
}

/// Compute aggregate commit duration: earliest run start to latest run finish.
/// Returns `None` when any selected run lacks parseable start/end timestamps.
fn compute_commit_duration_seconds(runs: &[ApiRun]) -> Option<u64> {
    if runs.is_empty() {
        return None;
    }

    let mut earliest_start: Option<jiff::Timestamp> = None;
    let mut latest_end: Option<jiff::Timestamp> = None;
    for run in runs {
        let start: jiff::Timestamp = run
            .run_started_at
            .as_deref()
            .or(run.created_at.as_deref())?
            .parse()
            .ok()?;
        let end: jiff::Timestamp = run.updated_at.as_deref()?.parse().ok()?;

        if earliest_start.is_none_or(|current| start < current) {
            earliest_start = Some(start);
        }
        if latest_end.is_none_or(|current| end > current) {
            latest_end = Some(end);
        }
    }

    let start = earliest_start?;
    let end = latest_end?;
    Some((end.as_second() - start.as_second()).max(0) as u64)
}

/// Compute a workflow run's wall-clock duration in seconds.
///
/// The start timestamp is `run_started_at` when present, falling back to
/// `created_at` when it is absent; the end is `updated_at` (the run's final
/// update, i.e. when it finished). Returns `None` when the selected start or the
/// end timestamp is missing or unparseable, so a duration is only ever reported
/// when it can be computed honestly. A negative span (clock skew between the two
/// timestamps) is clamped to `0` so the result is always non-negative.
fn compute_duration_seconds(
    run_started_at: Option<&str>,
    created_at: Option<&str>,
    updated_at: Option<&str>,
) -> Option<u64> {
    let start: jiff::Timestamp = run_started_at.or(created_at)?.parse().ok()?;
    let end: jiff::Timestamp = updated_at?.parse().ok()?;
    Some((end.as_second() - start.as_second()).max(0) as u64)
}

/// Normalize one API job into a [`JobReport`], or `None` if the job is not an
/// actionable problem (so it is not attached to the run). Only failed steps are
/// kept.
fn normalize_job(api: ApiJob) -> Option<JobReport> {
    let conclusion = Conclusion::from_github(api.conclusion.as_deref());
    if !conclusion.is_problem() {
        return None;
    }
    Some(JobReport {
        name: api.name.unwrap_or_default(),
        conclusion,
        url: api.html_url.unwrap_or_default(),
        steps: api.steps.into_iter().filter_map(normalize_step).collect(),
    })
}

/// Normalize one API step into a [`StepReport`], or `None` if the step is not an
/// actionable problem (passing/skipped steps are noise in a failure report).
fn normalize_step(api: ApiStep) -> Option<StepReport> {
    let conclusion = Conclusion::from_github(api.conclusion.as_deref());
    if !conclusion.is_problem() {
        return None;
    }
    Some(StepReport {
        name: api.name.unwrap_or_default(),
        number: api.number,
        conclusion,
    })
}

// --- HTTP helpers (pure, unit-testable) ------------------------------------

/// Percent-encode a query parameter value without adding another dependency.
fn percent_encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
        {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

/// Parse the `Link` header for the `rel="next"` URL, if present.
fn parse_next_link(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    parse_link_next(link)
}

/// Extract the `rel="next"` target from a raw `Link` header value.
///
/// e.g. `<https://api.github.com/...&page=2>; rel="next", <...>; rel="last"`.
fn parse_link_next(link: &str) -> Option<String> {
    for part in link.split(',') {
        let mut segments = part.split(';');
        let Some(url_seg) = segments.next() else {
            continue;
        };
        let is_next = segments
            .map(str::trim)
            .any(|s| s == "rel=\"next\"" || s == "rel=next");
        if is_next {
            let url =
                url_seg.trim().trim_start_matches('<').trim_end_matches('>');
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Whether a non-success response represents a (primary or secondary) rate
/// limit, which is retryable.
fn is_rate_limited(
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if status != StatusCode::FORBIDDEN {
        return false;
    }
    if header_str(headers, "x-ratelimit-remaining") == Some("0") {
        return true;
    }
    if headers.contains_key(reqwest::header::RETRY_AFTER) {
        return true;
    }
    let body = body.to_ascii_lowercase();
    body.contains("rate limit")
        || body.contains("secondary rate")
        || body.contains("abuse")
}

/// Parse a `Retry-After` header expressed in seconds.
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, "retry-after")?.trim().parse().ok()
}

/// Borrow a header value as `&str`, if present and valid UTF-8.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Turn a GitHub error body into a concise message: prefer the JSON `message`
/// field, else a trimmed/truncated body, else a placeholder.
fn error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
            if !message.trim().is_empty() {
                return message.trim().to_string();
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "(no response body)".to_string()
    } else {
        trimmed.chars().take(200).collect()
    }
}

// --- Resolution & fan-out --------------------------------------------------

/// The outcome of expanding a config into concrete repositories.
#[derive(Debug, Clone)]
pub struct ResolvedRepos {
    /// The merged, de-duplicated, alphabetically-stable repositories to inspect.
    pub repos: Vec<RepoName>,
    /// Orgs that failed to expand, as `(org, error message)`. Surfaced as
    /// error rows so a single failed org never aborts the run.
    pub org_errors: Vec<(String, String)>,
}

/// Expand every org in `config` (with bounded concurrency) and merge the
/// results with the explicitly-listed repos through [`resolve_repos`], yielding
/// one de-duplicated, sorted `owner/name` list.
///
/// A repo named both explicitly and via its org appears exactly once. Org
/// expansion failures are collected into [`ResolvedRepos::org_errors`] rather
/// than aborting; the caller decides how to surface them.
pub async fn resolve_repositories(
    client: &GitHubClient,
    config: &Config,
    concurrency: usize,
) -> ResolvedRepos {
    let expansions =
        futures::stream::iter(config.orgs().map(|org| async move {
            (org.org.clone(), client.list_org_repos(org).await)
        }))
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;

    let mut expanded = Vec::new();
    let mut org_errors = Vec::new();
    for (org, result) in expansions {
        match result {
            Ok(repos) => expanded.extend(repos),
            Err(e) => org_errors.push((org, e.to_string())),
        }
    }

    let explicit: Vec<RepoName> = config.explicit_repos().cloned().collect();
    ResolvedRepos {
        repos: resolve_repos(explicit, expanded),
        org_errors,
    }
}

/// Run a per-repository async operation across `repos` with bounded
/// concurrency, isolating each repo's failure into a [`RepoReport`] error
/// record so one failing repo never aborts the whole run.
///
/// `op` returns the repo's commits on success or a [`GitHubError`] on failure;
/// the error's message is stored in [`RepoReport::error`]. Results come back
/// sorted by `owner/name` for stable output regardless of completion order.
/// Phase 4 supplies the real `op` (select settled commits + enrich failed
/// runs); the fan-out, concurrency bound, and error isolation are owned here.
pub async fn fetch_repo_reports<F, Fut>(
    repos: Vec<RepoName>,
    concurrency: usize,
    op: F,
) -> Vec<RepoReport>
where
    F: Fn(RepoName) -> Fut,
    Fut: Future<Output = Result<Vec<CommitReport>, GitHubError>>,
{
    let mut reports: Vec<RepoReport> =
        futures::stream::iter(repos.into_iter().map(|repo| {
            let fut = op(repo.clone());
            async move {
                let name = repo.full_name();
                match fut.await {
                    Ok(commits) => RepoReport {
                        repo: name,
                        commits,
                        error: None,
                    },
                    Err(e) => RepoReport {
                        repo: name,
                        commits: vec![],
                        error: Some(e.to_string()),
                    },
                }
            }
        }))
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    reports.sort_by(|a, b| a.repo.cmp(&b.repo));
    reports
}

/// Case-insensitive repository comparison for GitHub-originated names.
fn repo_name_eq_ignore_ascii_case(a: &RepoName, b: &RepoName) -> bool {
    a.owner.eq_ignore_ascii_case(&b.owner)
        && a.name.eq_ignore_ascii_case(&b.name)
}

/// The Phase 4 collection pipeline: fan out over `repos` (bounded by
/// `concurrency`), fetching each repository's `limit` most-recent settled
/// commits on the default branch and enriching every problem run with its failed
/// jobs and steps.
///
/// Each repository is isolated by [`fetch_repo_reports`]: a repo that errors
/// (no access, Actions disabled, rate-limited, malformed at the HTTP layer)
/// becomes a [`RepoReport`] error row instead of aborting the run. A repo with
/// no settled commits returns an empty [`RepoReport`] for the CLI assembly step
/// to suppress consistently across renderers. Results come back sorted by
/// `owner/name`. Pair the returned `repos` with metadata to build a
/// [`crate::model::Report`] for rendering.
pub async fn collect_repo_reports(
    client: &GitHubClient,
    repos: Vec<RepoName>,
    limit: u32,
    concurrency: usize,
) -> Vec<RepoReport> {
    fetch_repo_reports(repos, concurrency, |repo| {
        let client = client.clone();
        async move { client.collect_commits(&repo, limit).await }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{
        method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn repo(owner: &str, name: &str) -> RepoName {
        RepoName {
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }

    fn org(name: &str) -> OrgSource {
        OrgSource {
            org: name.to_string(),
            include_archived: false,
            include_forks: false,
            exclude: vec![],
        }
    }

    fn test_client(base_url: &str) -> GitHubClient {
        GitHubClient::new(base_url, None, RetryConfig::no_delay(3))
            .expect("client builds")
    }

    /// JSON body for one repo in the org-repos listing.
    fn repo_json(owner: &str, name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "owner": { "login": owner } })
    }

    // --- Token discovery -------------------------------------------------

    #[test]
    fn token_prefers_actstat_var_over_others() {
        let env = |k: &str| match k {
            "ACTSTAT_GITHUB_TOKEN" => Some("actstat-tok".to_string()),
            "GH_TOKEN" => Some("gh-tok".to_string()),
            _ => None,
        };
        let d = discover_token_with(env, || panic!("gh must not be called"));
        assert_eq!(d.token.as_deref(), Some("actstat-tok"));
        assert_eq!(d.source, TokenSource::Env("ACTSTAT_GITHUB_TOKEN".into()));
        assert!(d.is_authenticated());
        assert!(d.unauthenticated_warning().is_none());
    }

    #[test]
    fn token_falls_back_gh_token_then_github_token() {
        let only_github =
            |k: &str| (k == "GITHUB_TOKEN").then(|| "github-tok".to_string());
        let d = discover_token_with(only_github, || None);
        assert_eq!(d.token.as_deref(), Some("github-tok"));
        assert_eq!(d.source, TokenSource::Env("GITHUB_TOKEN".into()));
    }

    #[test]
    fn token_falls_back_to_gh_cli() {
        let d = discover_token_with(|_| None, || Some("  cli-tok\n".into()));
        assert_eq!(d.token.as_deref(), Some("cli-tok"));
        assert_eq!(d.source, TokenSource::GhCli);
    }

    #[test]
    fn token_blank_env_value_is_ignored() {
        // A set-but-empty var must not shadow a real token further down.
        let env = |k: &str| match k {
            "ACTSTAT_GITHUB_TOKEN" => Some("   ".to_string()),
            "GH_TOKEN" => Some("real".to_string()),
            _ => None,
        };
        let d = discover_token_with(env, || None);
        assert_eq!(d.token.as_deref(), Some("real"));
    }

    #[test]
    fn token_unauthenticated_when_nothing_found() {
        let d = discover_token_with(|_| None, || None);
        assert!(!d.is_authenticated());
        assert_eq!(d.source, TokenSource::Unauthenticated);
        assert!(d.unauthenticated_warning().is_some());
    }

    // --- Pure HTTP helpers ----------------------------------------------

    #[test]
    fn link_header_next_is_extracted() {
        let link =
            "<https://api.github.com/orgs/x/repos?page=2>; rel=\"next\", \
                    <https://api.github.com/orgs/x/repos?page=5>; rel=\"last\"";
        assert_eq!(
            parse_link_next(link).as_deref(),
            Some("https://api.github.com/orgs/x/repos?page=2")
        );
    }

    #[test]
    fn link_header_without_next_is_none() {
        let link = "<https://api.github.com/orgs/x/repos?page=1>; rel=\"prev\"";
        assert_eq!(parse_link_next(link), None);
    }

    #[test]
    fn error_message_prefers_json_message_field() {
        assert_eq!(
            error_message(r#"{"message":"Not Found","x":1}"#),
            "Not Found"
        );
    }

    #[test]
    fn error_message_falls_back_to_trimmed_body() {
        assert_eq!(error_message("  oops  "), "oops");
        assert_eq!(error_message(""), "(no response body)");
    }

    #[test]
    fn rate_limit_detection_covers_429_and_exhausted_403() {
        let empty = HeaderMap::new();
        assert!(is_rate_limited(StatusCode::TOO_MANY_REQUESTS, &empty, ""));

        let mut exhausted = HeaderMap::new();
        exhausted.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert!(is_rate_limited(StatusCode::FORBIDDEN, &exhausted, ""));

        // A plain 403 with no rate-limit signal is *not* a rate limit.
        assert!(!is_rate_limited(StatusCode::FORBIDDEN, &empty, "nope"));

        // Secondary ("abuse") limits are detected from the body.
        assert!(is_rate_limited(
            StatusCode::FORBIDDEN,
            &empty,
            "You have exceeded a secondary rate limit"
        ));
    }

    // --- Org expansion (mocked HTTP) ------------------------------------

    #[tokio::test]
    async fn list_org_repos_single_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/sase-org/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    repo_json("sase-org", "tools"),
                    repo_json("sase-org", "infra"),
                ]),
            ))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let repos = client.list_org_repos(&org("sase-org")).await.unwrap();
        assert_eq!(
            repos,
            vec![repo("sase-org", "tools"), repo("sase-org", "infra")]
        );
    }

    #[tokio::test]
    async fn list_org_repos_follows_pagination() {
        let server = MockServer::start().await;
        let next = format!(
            "<{}/orgs/big/repos?per_page=100&page=2>; rel=\"next\"",
            server.uri()
        );
        // Page 1 (no `page` param) carries a Link header to page 2.
        Mock::given(method("GET"))
            .and(path("/orgs/big/repos"))
            .and(query_param_is_missing("page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", next.as_str())
                    .set_body_json(serde_json::json!([repo_json("big", "a")])),
            )
            .mount(&server)
            .await;
        // Page 2 (no further Link) ends pagination.
        Mock::given(method("GET"))
            .and(path("/orgs/big/repos"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([repo_json("big", "b")])),
            )
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let repos = client.list_org_repos(&org("big")).await.unwrap();
        assert_eq!(repos, vec![repo("big", "a"), repo("big", "b")]);
    }

    #[tokio::test]
    async fn list_org_repos_applies_filters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/sase-org/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    { "name": "active", "owner": { "login": "sase-org" } },
                    { "name": "old", "owner": { "login": "sase-org" }, "archived": true },
                    { "name": "mirror", "owner": { "login": "sase-org" }, "fork": true },
                    { "name": "scratch", "owner": { "login": "sase-org" } },
                ]),
            ))
            .mount(&server)
            .await;

        let mut source = org("sase-org");
        source.exclude = vec![repo("Sase-Org", "Scratch")];
        let client = test_client(&server.uri());
        let repos = client.list_org_repos(&source).await.unwrap();
        // archived, fork, and case-mismatched excluded entries are dropped by
        // default.
        assert_eq!(repos, vec![repo("sase-org", "active")]);
    }

    // --- Error mapping (mocked HTTP) ------------------------------------

    #[tokio::test]
    async fn unauthorized_maps_to_unauthorized_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                serde_json::json!({ "message": "Bad credentials" }),
            ))
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let err = client.list_org_repos(&org("x")).await.unwrap_err();
        assert!(matches!(err, GitHubError::Unauthorized(_)));
        assert!(err.to_string().contains("Bad credentials"));
    }

    #[tokio::test]
    async fn not_found_maps_to_not_found_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(
                    serde_json::json!({ "message": "Not Found" }),
                ),
            )
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let err = client.list_org_repos(&org("ghost")).await.unwrap_err();
        assert!(matches!(err, GitHubError::NotFound(_)));
    }

    #[tokio::test]
    async fn plain_forbidden_maps_to_forbidden_not_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                serde_json::json!({ "message": "Resource not accessible" }),
            ))
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let err = client.list_org_repos(&org("locked")).await.unwrap_err();
        assert!(matches!(err, GitHubError::Forbidden(_)));
    }

    #[tokio::test]
    async fn rate_limit_403_is_retried_then_surfaced() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("retry-after", "1")
                    .set_body_json(serde_json::json!({ "message": "API rate limit exceeded" })),
            )
            .mount(&server)
            .await;
        // no_delay(2) → 1 initial attempt + 2 retries = 3 requests.
        let client = test_client(&server.uri());
        let err = client.list_org_repos(&org("x")).await.unwrap_err();
        assert!(matches!(err, GitHubError::RateLimited { .. }));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "1 attempt + 3 retries");
    }

    #[tokio::test]
    async fn server_error_is_retried_then_recovers() {
        let server = MockServer::start().await;
        let body = serde_json::json!([repo_json("flaky", "ok")]).to_string();
        Mock::given(method("GET"))
            .respond_with(FlakyThenOk::new(2, body))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let repos = client.list_org_repos(&org("flaky")).await.unwrap();
        assert_eq!(repos, vec![repo("flaky", "ok")]);
        // 2 failing attempts then a success.
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    /// A responder that returns `500` for the first `fail_times` calls, then a
    /// `200` with `ok_body`. Used to test transient-failure recovery
    /// deterministically without relying on mock ordering.
    struct FlakyThenOk {
        fail_times: usize,
        calls: std::sync::atomic::AtomicUsize,
        ok_body: String,
    }

    impl FlakyThenOk {
        fn new(fail_times: usize, ok_body: String) -> Self {
            FlakyThenOk {
                fail_times,
                calls: std::sync::atomic::AtomicUsize::new(0),
                ok_body,
            }
        }
    }

    impl Respond for FlakyThenOk {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n =
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_times {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(self.ok_body.clone())
            }
        }
    }

    // --- Cross-source resolution (mocked HTTP) --------------------------

    #[tokio::test]
    async fn resolve_dedups_orgs_and_explicit_repos() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/sase-org/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    repo_json("sase-org", "tools"),
                    repo_json("sase-org", "infra"),
                ]),
            ))
            .mount(&server)
            .await;

        let config = Config {
            projects: vec![
                crate::config::ProjectSource::Org(org("sase-org")),
                // Listed explicitly *and* surfaced by the org: must collapse.
                crate::config::ProjectSource::Repo(repo("sase-org", "tools")),
                crate::config::ProjectSource::Repo(repo(
                    "bbugyi200",
                    "actstat",
                )),
            ],
        };

        let client = test_client(&server.uri());
        let resolved = resolve_repositories(&client, &config, 8).await;
        assert!(resolved.org_errors.is_empty());
        let names: Vec<String> =
            resolved.repos.iter().map(RepoName::full_name).collect();
        assert_eq!(
            names,
            vec![
                "bbugyi200/actstat".to_string(),
                "sase-org/infra".to_string(),
                "sase-org/tools".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_isolates_a_failing_org() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/orgs/good/repos"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([repo_json("good", "a")])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/orgs/bad/repos"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(
                    serde_json::json!({ "message": "Not Found" }),
                ),
            )
            .mount(&server)
            .await;

        let config = Config {
            projects: vec![
                crate::config::ProjectSource::Org(org("good")),
                crate::config::ProjectSource::Org(org("bad")),
            ],
        };
        let client = test_client(&server.uri());
        let resolved = resolve_repositories(&client, &config, 8).await;

        // The good org still resolves; the bad org becomes an error row.
        assert_eq!(resolved.repos, vec![repo("good", "a")]);
        assert_eq!(resolved.org_errors.len(), 1);
        assert_eq!(resolved.org_errors[0].0, "bad");
        assert!(resolved.org_errors[0].1.contains("404"));
    }

    // --- Per-repo fan-out & isolation -----------------------------------

    #[tokio::test]
    async fn fan_out_isolates_failures_and_sorts() {
        // Pure-closure op: every-other repo "fails".
        let repos = vec![repo("o", "c"), repo("o", "a"), repo("o", "b")];
        let reports = fetch_repo_reports(repos, 2, |r: RepoName| async move {
            if r.name == "b" {
                Err(GitHubError::Forbidden("nope".into()))
            } else {
                Ok(vec![])
            }
        })
        .await;

        let names: Vec<&str> =
            reports.iter().map(|r| r.repo.as_str()).collect();
        assert_eq!(names, vec!["o/a", "o/b", "o/c"], "stable sorted order");
        let b = reports.iter().find(|r| r.repo == "o/b").unwrap();
        assert!(b.error.as_deref().unwrap().contains("forbidden"));
        assert!(reports.iter().filter(|r| r.error.is_none()).count() == 2);
    }

    #[tokio::test]
    async fn fan_out_isolates_per_repo_http_failures() {
        // A real client against mock HTTP: one repo OK, others 404/403.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/ok/actions/runs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/missing/actions/runs"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(
                    serde_json::json!({ "message": "Not Found" }),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/locked/actions/runs"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(
                    serde_json::json!({ "message": "Forbidden" }),
                ),
            )
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let repos =
            vec![repo("o", "ok"), repo("o", "missing"), repo("o", "locked")];
        let reports = fetch_repo_reports(repos, 8, |r: RepoName| {
            let client = client.clone();
            async move {
                let path = format!("/repos/{}/actions/runs", r.full_name());
                client.get::<serde_json::Value>(&path).await?;
                Ok(vec![])
            }
        })
        .await;

        let ok = reports.iter().find(|r| r.repo == "o/ok").unwrap();
        assert!(ok.error.is_none());
        let missing = reports.iter().find(|r| r.repo == "o/missing").unwrap();
        assert!(missing.error.as_deref().unwrap().contains("404"));
        let locked = reports.iter().find(|r| r.repo == "o/locked").unwrap();
        assert!(locked.error.as_deref().unwrap().contains("403"));
    }

    // --- Run & failure-detail collection (Phase 4) ----------------------

    /// A full, well-formed workflow run with the given id and conclusion.
    fn run_json(id: u64, conclusion: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "workflow_id": id,
            "name": "CI",
            "display_title": "Tidy up the build",
            "run_number": id,
            "event": "push",
            "head_branch": "main",
            "head_sha": "abcdef1234567890deadbeef",
            "status": "completed",
            "conclusion": conclusion,
            "html_url": format!("https://github.com/o/r/actions/runs/{id}"),
            "created_at": "2026-06-29T11:50:00Z",
            "updated_at": "2026-06-29T11:52:30Z",
        })
    }

    fn run_json_with(
        id: u64,
        workflow_id: u64,
        workflow: &str,
        sha: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "workflow_id": workflow_id,
            "name": workflow,
            "display_title": format!("Commit {sha}"),
            "run_number": id,
            "event": "push",
            "head_branch": "main",
            "head_sha": sha,
            "status": status,
            "conclusion": conclusion,
            "html_url": format!("https://github.com/o/r/actions/runs/{id}"),
            "created_at": "2026-06-29T11:50:00Z",
            "updated_at": "2026-06-29T11:52:30Z",
        })
    }

    fn api_run(
        id: u64,
        workflow_id: u64,
        sha: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> ApiRun {
        serde_json::from_value(run_json_with(
            id,
            workflow_id,
            "CI",
            sha,
            status,
            conclusion,
        ))
        .expect("test run JSON matches ApiRun")
    }

    /// The runs-listing envelope around a set of runs.
    fn runs_body(runs: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "total_count": runs.len(), "workflow_runs": runs })
    }

    /// A job with the given name/conclusion and explicit steps.
    fn job_json(
        name: &str,
        conclusion: &str,
        steps: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "conclusion": conclusion,
            "html_url": format!("https://github.com/o/r/jobs/{name}"),
            "steps": steps,
        })
    }

    /// A single step record.
    fn step_json(
        number: u64,
        name: &str,
        conclusion: &str,
    ) -> serde_json::Value {
        serde_json::json!({ "number": number, "name": name, "conclusion": conclusion })
    }

    /// The jobs-listing envelope around a set of jobs.
    fn jobs_body(jobs: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "total_count": jobs.len(), "jobs": jobs })
    }

    /// Mount a repository detail response for `owner/name`.
    async fn mount_repo_detail(
        server: &MockServer,
        owner: &str,
        name: &str,
        default_branch: &str,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{owner}/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "default_branch": default_branch }),
            ))
            .mount(server)
            .await;
    }

    /// Mount a default-branch runs-listing response for `owner/name`.
    async fn mount_runs(
        server: &MockServer,
        owner: &str,
        name: &str,
        body: serde_json::Value,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{owner}/{name}/actions/runs")))
            .and(query_param("branch", "main"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Mount a jobs-listing response for a specific run id.
    async fn mount_jobs(
        server: &MockServer,
        owner: &str,
        name: &str,
        run_id: u64,
        body: serde_json::Value,
    ) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{owner}/{name}/actions/runs/{run_id}/jobs"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    // --- Pure normalization ---------------------------------------------

    #[test]
    fn normalize_run_shortens_sha_and_maps_fields() {
        let (id, run) = normalize_run(ApiRun {
            id: 7,
            workflow_id: Some(10),
            name: Some("CI".into()),
            display_title: Some("Fix it".into()),
            run_number: 42,
            event: Some("pull_request".into()),
            head_branch: Some("feature/x".into()),
            head_sha: Some("abcdef1234567890".into()),
            status: Some("completed".into()),
            conclusion: Some("success".into()),
            html_url: Some("https://example/run".into()),
            created_at: Some("2026-06-29T11:50:00Z".into()),
            run_started_at: None,
            updated_at: Some("2026-06-29T11:52:30Z".into()),
        });
        assert_eq!(id, 7);
        assert_eq!(run.workflow, "CI");
        assert_eq!(run.branch, "feature/x");
        assert_eq!(run.sha, "abcdef1", "head SHA is shortened to 7 chars");
        assert_eq!(run.run_number, 42);
        assert_eq!(run.conclusion, Conclusion::Success);
        // With no run_started_at, duration falls back to created_at → updated_at
        // (11:50:00 → 11:52:30 = 150s).
        assert_eq!(run.duration_seconds, Some(150));
        assert!(run.jobs.is_empty());
    }

    #[test]
    fn compute_duration_prefers_run_started_at() {
        // run_started_at (11:51:00) is used over created_at (11:50:00):
        // 11:51:00 → 11:52:30 = 90s.
        let secs = compute_duration_seconds(
            Some("2026-06-29T11:51:00Z"),
            Some("2026-06-29T11:50:00Z"),
            Some("2026-06-29T11:52:30Z"),
        );
        assert_eq!(secs, Some(90));
    }

    #[test]
    fn compute_duration_falls_back_to_created_at() {
        // Absent run_started_at → created_at is the start (11:50:00 → 11:52:30).
        let secs = compute_duration_seconds(
            None,
            Some("2026-06-29T11:50:00Z"),
            Some("2026-06-29T11:52:30Z"),
        );
        assert_eq!(secs, Some(150));
    }

    #[test]
    fn compute_duration_is_none_when_timestamps_missing() {
        // No start at all.
        assert_eq!(
            compute_duration_seconds(None, None, Some("2026-06-29T11:52:30Z")),
            None
        );
        // No end.
        assert_eq!(
            compute_duration_seconds(Some("2026-06-29T11:50:00Z"), None, None),
            None
        );
    }

    #[test]
    fn compute_duration_is_none_for_malformed_timestamps() {
        // An unparseable start or end yields no duration rather than a guess.
        assert_eq!(
            compute_duration_seconds(
                Some("not-a-time"),
                None,
                Some("2026-06-29T11:52:30Z")
            ),
            None
        );
        assert_eq!(
            compute_duration_seconds(
                Some("2026-06-29T11:50:00Z"),
                None,
                Some("nonsense")
            ),
            None
        );
    }

    #[test]
    fn compute_duration_clamps_negative_span_to_zero() {
        // updated_at before the start (clock skew) clamps to 0, never negative.
        let secs = compute_duration_seconds(
            Some("2026-06-29T11:52:30Z"),
            None,
            Some("2026-06-29T11:50:00Z"),
        );
        assert_eq!(secs, Some(0));
    }

    #[test]
    fn normalize_job_drops_passing_jobs_and_passing_steps() {
        // A passing job is not a problem and is dropped entirely.
        assert!(normalize_job(ApiJob {
            name: Some("build".into()),
            conclusion: Some("success".into()),
            html_url: None,
            steps: vec![],
        })
        .is_none());

        // A failed job keeps only its actionable (non-passing/non-skipped)
        // steps.
        let job = normalize_job(ApiJob {
            name: Some("test".into()),
            conclusion: Some("failure".into()),
            html_url: Some("https://example/job".into()),
            steps: vec![
                ApiStep {
                    name: Some("Checkout".into()),
                    number: 1,
                    conclusion: Some("success".into()),
                },
                ApiStep {
                    name: Some("Run tests".into()),
                    number: 5,
                    conclusion: Some("failure".into()),
                },
                ApiStep {
                    name: Some("Upload".into()),
                    number: 6,
                    conclusion: Some("skipped".into()),
                },
            ],
        })
        .expect("a failed job is kept");
        assert_eq!(job.name, "test");
        assert_eq!(job.steps.len(), 1, "only the failed step survives");
        assert_eq!(job.steps[0].name, "Run tests");
        assert_eq!(job.steps[0].number, 5);
    }

    #[test]
    fn is_problem_excludes_success_skipped_neutral() {
        assert!(!Conclusion::Success.is_problem());
        assert!(!Conclusion::Skipped.is_problem());
        assert!(!Conclusion::Neutral.is_problem());
        for c in [
            Conclusion::Failure,
            Conclusion::Cancelled,
            Conclusion::TimedOut,
            Conclusion::ActionRequired,
            Conclusion::StartupFailure,
            Conclusion::Stale,
        ] {
            assert!(c.is_problem(), "{c:?} is an actionable problem");
        }
    }

    // --- Settled-commit selection ---------------------------------------

    #[test]
    fn select_settled_commits_skips_unsettled_newer_commits_and_applies_limit()
    {
        let runs = vec![
            api_run(5, 1, "newest", "queued", None),
            api_run(4, 1, "settled-red", "completed", Some("failure")),
            api_run(3, 2, "settled-red", "completed", Some("success")),
            api_run(2, 1, "settled-green", "completed", Some("success")),
        ];

        let selected = select_settled_commits(runs, 1);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].full_sha, "settled-red");
        assert_eq!(selected[0].runs.len(), 2);
    }

    #[test]
    fn select_settled_commits_keeps_latest_run_per_workflow() {
        let runs = vec![
            api_run(10, 1, "abc", "completed", Some("success")),
            api_run(12, 1, "abc", "completed", Some("failure")),
            api_run(11, 2, "abc", "completed", Some("success")),
        ];

        let selected = select_settled_commits(runs, 1);
        let ids: Vec<u64> = selected[0].runs.iter().map(|run| run.id).collect();

        assert_eq!(ids, vec![12, 11]);
    }

    #[test]
    fn select_settled_commits_treats_latest_rerun_status_as_authoritative() {
        let runs = vec![
            api_run(10, 1, "abc", "completed", Some("failure")),
            api_run(12, 1, "abc", "in_progress", None),
        ];

        let selected = select_settled_commits(runs, 1);

        assert!(
            selected.is_empty(),
            "newer in-progress rerun makes the commit unsettled"
        );
    }

    // --- Commit collection (mocked HTTP) --------------------------------

    #[tokio::test]
    async fn passing_commit_needs_no_jobs_request() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "r", "main").await;
        mount_runs(&server, "o", "r", runs_body(vec![run_json(1, "success")]))
            .await;
        // Deliberately mount NO jobs endpoint: a successful run must not trigger
        // one, so collection still succeeds.

        let client = test_client(&server.uri());
        let commits = client.collect_commits(&repo("o", "r"), 1).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert!(commits[0].is_success());
        assert!(
            commits[0].runs[0].jobs.is_empty(),
            "non-problem runs carry no job expansion"
        );

        // Prove the jobs endpoint was never called.
        let hit_jobs = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().contains("/jobs"));
        assert!(!hit_jobs, "no jobs request for a successful run");
    }

    #[tokio::test]
    async fn failed_commit_is_enriched_with_failed_jobs_and_steps() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "r", "main").await;
        mount_runs(&server, "o", "r", runs_body(vec![run_json(2, "failure")]))
            .await;
        mount_jobs(
            &server,
            "o",
            "r",
            2,
            jobs_body(vec![
                // A passing job must be dropped from the failure detail.
                job_json("build", "success", vec![]),
                // The failed job is kept, with only its failed step.
                job_json(
                    "test (3.14)",
                    "failure",
                    vec![
                        step_json(1, "Checkout", "success"),
                        step_json(7, "Run tests", "failure"),
                        step_json(8, "Coverage", "skipped"),
                    ],
                ),
            ]),
        )
        .await;

        let client = test_client(&server.uri());
        let commits = client.collect_commits(&repo("o", "r"), 1).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].conclusion, Conclusion::Failure);
        let run = &commits[0].runs[0];
        assert_eq!(run.conclusion, Conclusion::Failure);
        assert_eq!(
            run.jobs.len(),
            1,
            "only the non-successful job is attached"
        );
        let job = &run.jobs[0];
        assert_eq!(job.name, "test (3.14)");
        assert_eq!(job.conclusion, Conclusion::Failure);
        assert!(job.url.contains("/jobs/"), "job URL preserved");
        assert_eq!(job.steps.len(), 1, "only the failed step is attached");
        assert_eq!(job.steps[0].name, "Run tests");
        assert_eq!(job.steps[0].number, 7);
    }

    #[tokio::test]
    async fn all_problem_runs_on_a_commit_are_enriched() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "r", "main").await;
        mount_runs(
            &server,
            "o",
            "r",
            runs_body(vec![
                run_json(1, "success"),
                run_json(2, "cancelled"),
                run_json(3, "timed_out"),
            ]),
        )
        .await;
        // Only the two unhealthy runs get a jobs request (id 1 is successful).
        mount_jobs(
            &server,
            "o",
            "r",
            2,
            jobs_body(vec![job_json("deploy", "cancelled", vec![])]),
        )
        .await;
        mount_jobs(
            &server,
            "o",
            "r",
            3,
            jobs_body(vec![job_json(
                "integration",
                "timed_out",
                vec![step_json(4, "Wait for service", "timed_out")],
            )]),
        )
        .await;

        let client = test_client(&server.uri());
        let commits = client.collect_commits(&repo("o", "r"), 1).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].runs.len(), 3);
        assert_eq!(commits[0].conclusion, Conclusion::Failure);

        let by_number = |n: u64| {
            commits[0].runs.iter().find(|r| r.run_number == n).unwrap()
        };
        assert!(!by_number(1).is_problem());
        assert!(by_number(1).jobs.is_empty());

        let cancelled = by_number(2);
        assert_eq!(cancelled.conclusion, Conclusion::Cancelled);
        assert_eq!(cancelled.jobs.len(), 1);

        let timed_out = by_number(3);
        assert_eq!(timed_out.conclusion, Conclusion::TimedOut);
        assert_eq!(timed_out.jobs[0].steps[0].conclusion, Conclusion::TimedOut);
    }

    #[tokio::test]
    async fn no_settled_commits_is_an_empty_report_for_later_suppression() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "empty", "main").await;
        mount_runs(
            &server,
            "o",
            "empty",
            runs_body(vec![run_json_with(1, 1, "CI", "abc", "queued", None)]),
        )
        .await;

        let client = test_client(&server.uri());
        let reports =
            collect_repo_reports(&client, vec![repo("o", "empty")], 1, 8).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].repo, "o/empty");
        assert!(
            reports[0].commits.is_empty(),
            "no settled commits is not an error"
        );
        assert!(reports[0].error.is_none(), "empty is not an error");
    }

    #[tokio::test]
    async fn malformed_run_payload_degrades_to_defaults() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "r", "main").await;
        // A partial run: only an id and a (passing) conclusion; every other
        // field is absent. It must not fail the repo.
        mount_runs(
            &server,
            "o",
            "r",
            serde_json::json!({
                "workflow_runs": [
                    { "id": 99, "status": "completed", "conclusion": "success" }
                ]
            }),
        )
        .await;

        let client = test_client(&server.uri());
        let commits = client.collect_commits(&repo("o", "r"), 1).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].branch, "main");
        let run = &commits[0].runs[0];
        assert_eq!(run.conclusion, Conclusion::Success);
        assert_eq!(run.workflow, "", "missing fields default cleanly");
        assert_eq!(run.branch, "");
        assert_eq!(run.sha, "");
        assert_eq!(run.run_number, 0);
        assert!(run.jobs.is_empty());
    }

    #[tokio::test]
    async fn collect_commits_sends_default_branch_and_per_page() {
        let server = MockServer::start().await;
        mount_repo_detail(&server, "o", "r", "main").await;
        // The request must carry branch=main and per_page=100, with no
        // status=completed filter.
        Mock::given(method("GET"))
            .and(path("/repos/o/r/actions/runs"))
            .and(query_param("branch", "main"))
            .and(query_param("per_page", "100"))
            .and(query_param_is_missing("status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(runs_body(vec![run_json(1, "success")])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let commits = client.collect_commits(&repo("o", "r"), 3).await.unwrap();
        assert_eq!(commits.len(), 1);
        // The `.expect(1)` on the mock asserts the query was exactly right.
    }

    #[tokio::test]
    async fn list_run_jobs_keeps_only_failed_jobs_and_failed_steps() {
        let server = MockServer::start().await;
        mount_jobs(
            &server,
            "o",
            "r",
            5,
            jobs_body(vec![
                job_json("lint", "success", vec![]),
                job_json("skipped-job", "skipped", vec![]),
                job_json(
                    "test",
                    "failure",
                    vec![
                        step_json(1, "Setup", "success"),
                        step_json(2, "Test", "failure"),
                    ],
                ),
            ]),
        )
        .await;

        let client = test_client(&server.uri());
        let jobs = client.list_run_jobs(&repo("o", "r"), 5).await.unwrap();
        assert_eq!(jobs.len(), 1, "only the failed job is kept");
        assert_eq!(jobs[0].name, "test");
        assert_eq!(jobs[0].steps.len(), 1);
        assert_eq!(jobs[0].steps[0].name, "Test");
    }

    // --- End-to-end collection → stable JSON ----------------------------

    #[tokio::test]
    async fn collect_repo_reports_renders_stable_json() {
        let server = MockServer::start().await;
        // A passing repo and a failing repo, fanned out together.
        mount_repo_detail(&server, "o", "pass", "main").await;
        mount_runs(
            &server,
            "o",
            "pass",
            runs_body(vec![run_json(1, "success")]),
        )
        .await;
        mount_repo_detail(&server, "o", "fail", "main").await;
        mount_runs(
            &server,
            "o",
            "fail",
            runs_body(vec![run_json(2, "failure")]),
        )
        .await;
        mount_jobs(
            &server,
            "o",
            "fail",
            2,
            jobs_body(vec![job_json(
                "test",
                "failure",
                vec![step_json(3, "Run tests", "failure")],
            )]),
        )
        .await;

        let client = test_client(&server.uri());
        let repos = collect_repo_reports(
            &client,
            vec![repo("o", "fail"), repo("o", "pass")],
            1,
            8,
        )
        .await;

        // Build a Report (deterministic metadata) and render JSON.
        let report = crate::model::Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit: 1,
            repos,
        };
        let rendered = crate::render::json(&report);
        let value: serde_json::Value = serde_json::from_str(&rendered)
            .expect("collection renders valid JSON");

        let repos = value["repositories"].as_array().unwrap();
        assert_eq!(repos.len(), 2);
        // Sorted by owner/name: "o/fail" before "o/pass".
        assert_eq!(repos[0]["repo"], "o/fail");
        assert_eq!(repos[1]["repo"], "o/pass");

        // Passing repo: one green commit, one passing run, no job expansion.
        let pass_commit = &repos[1]["commits"][0];
        assert_eq!(pass_commit["conclusion"], "success");
        let pass_run = &pass_commit["runs"][0];
        assert_eq!(pass_run["conclusion"], "success");
        assert_eq!(pass_run["jobs"].as_array().unwrap().len(), 0);

        // Failing repo: red commit with failed job + failed step preserved.
        let fail_commit = &repos[0]["commits"][0];
        assert_eq!(fail_commit["conclusion"], "failure");
        let fail_run = &fail_commit["runs"][0];
        assert_eq!(fail_run["conclusion"], "failure");
        assert_eq!(fail_run["jobs"][0]["name"], "test");
        assert_eq!(fail_run["jobs"][0]["steps"][0]["name"], "Run tests");
        assert!(report_has_failures(&value));
    }

    /// Tiny helper mirroring `Report::has_failures` over the rendered JSON.
    fn report_has_failures(value: &serde_json::Value) -> bool {
        value["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| {
                repo["commits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|commit| commit["conclusion"] != "success")
            })
    }
}
