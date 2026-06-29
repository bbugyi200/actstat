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
//!    following pagination to exhaustion and honoring the reserved filters.
//! 4. [`resolve_repositories`] — expand every configured org (bounded
//!    concurrency), then merge with the explicit repos through Phase 2's
//!    [`resolve_repos`] into one de-duplicated, alphabetically-stable list.
//! 5. [`fetch_repo_reports`] — run a per-repository async operation across many
//!    repos with bounded concurrency, isolating each repo's failure into a
//!    [`RepoReport`] error record so one bad repo never aborts the whole run.
//!    Phase 4 supplies the real operation (fetch runs + enrich failures); the
//!    fan-out, the concurrency bound, and the error isolation live here.

use std::future::Future;
use std::time::Duration;

use futures::stream::StreamExt as _;
use reqwest::header::HeaderMap;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use thiserror::Error;

use crate::config::{resolve_repos, Config, OrgSource, RepoName};
use crate::model::{RepoReport, RunReport};

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
    /// Follows pagination to exhaustion and applies the org's reserved filters:
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
            .filter(|name| !org.exclude.contains(name))
            .collect())
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

// --- HTTP helpers (pure, unit-testable) ------------------------------------

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
/// results with the explicitly-listed repos through Phase 2's [`resolve_repos`],
/// yielding one de-duplicated, sorted `owner/name` list.
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
/// `op` returns the repo's runs on success or a [`GitHubError`] on failure; the
/// error's message is stored in [`RepoReport::error`]. Results come back sorted
/// by `owner/name` for stable output regardless of completion order. Phase 4
/// supplies the real `op` (fetch completed runs + enrich non-successful ones);
/// the fan-out, concurrency bound, and error isolation are owned here.
pub async fn fetch_repo_reports<F, Fut>(
    repos: Vec<RepoName>,
    concurrency: usize,
    op: F,
) -> Vec<RepoReport>
where
    F: Fn(RepoName) -> Fut,
    Fut: Future<Output = Result<Vec<RunReport>, GitHubError>>,
{
    let mut reports: Vec<RepoReport> =
        futures::stream::iter(repos.into_iter().map(|repo| {
            let fut = op(repo.clone());
            async move {
                let name = repo.full_name();
                match fut.await {
                    Ok(runs) => RepoReport {
                        repo: name,
                        runs,
                        error: None,
                    },
                    Err(e) => RepoReport {
                        repo: name,
                        runs: vec![],
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
        source.exclude = vec![repo("sase-org", "scratch")];
        let client = test_client(&server.uri());
        let repos = client.list_org_repos(&source).await.unwrap();
        // archived, fork, and excluded entries are dropped by default.
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
}
