//! The single normalized result model that every output format renders from.
//!
//! `Report -> [RepoReport] -> [RunReport] -> [JobReport] -> [StepReport]`.
//! GitHub parsing (Phases 3–4) populates this tree; renderers (Phase 5) only
//! ever read it. Phase 1 ships a deterministic [`Report::stub`] so all three
//! output formats can be exercised end-to-end without any network access.

use serde::Serialize;

/// Top-level report: metadata plus one entry per inspected repository.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// When the report was generated (RFC3339). Stubbed deterministically in
    /// Phase 1 so output is reproducible in tests.
    pub generated_at: String,
    /// The per-repository run limit that was requested (`-n/--limit`).
    pub limit: u32,
    /// Repositories, expected to be sorted alphabetically by `repo`.
    #[serde(rename = "repositories")]
    pub repos: Vec<RepoReport>,
}

/// One repository's worth of results, or a per-repository error.
#[derive(Debug, Clone, Serialize)]
pub struct RepoReport {
    /// `owner/name`.
    pub repo: String,
    /// The most-recent completed runs, newest first. Empty is a valid,
    /// neutral state ("no completed runs").
    pub runs: Vec<RunReport>,
    /// A per-repository error (no access, Actions disabled, rate-limited, …).
    /// Present errors never abort the whole run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single workflow run, normalized from the GitHub API.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// Workflow name (e.g. `CI`).
    pub workflow: String,
    /// The run's display title (commit message / PR title).
    pub title: String,
    /// Monotonic run number within the workflow.
    pub run_number: u64,
    /// Triggering event (`push`, `pull_request`, `schedule`, …).
    pub event: String,
    /// Head branch the run executed against.
    pub branch: String,
    /// Short (7-char) head SHA.
    pub sha: String,
    /// Terminal conclusion of the completed run.
    pub conclusion: Conclusion,
    /// Direct URL to the run on github.com.
    pub url: String,
    /// When the run was created (RFC3339).
    pub created_at: String,
    /// When the run last updated, i.e. finished (RFC3339).
    pub updated_at: String,
    /// Only *non-successful* jobs are attached; empty for passing runs.
    pub jobs: Vec<JobReport>,
}

/// A single (non-successful) job within a run.
#[derive(Debug, Clone, Serialize)]
pub struct JobReport {
    /// Job name (e.g. `test (3.14)`).
    pub name: String,
    /// The job's conclusion.
    pub conclusion: Conclusion,
    /// Direct URL to the job log.
    pub url: String,
    /// Only *failed* steps are attached.
    pub steps: Vec<StepReport>,
}

/// A single (failed) step within a job.
#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    /// Step name (e.g. `Run tests`).
    pub name: String,
    /// 1-based step number within the job.
    pub number: u64,
    /// The step's conclusion.
    pub conclusion: Conclusion,
}

/// Normalized workflow-run conclusion. `success` is the only passing variant;
/// every other variant is rendered with its own label/icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// The run passed.
    Success,
    /// The run failed.
    Failure,
    /// The run was cancelled.
    Cancelled,
    /// The run timed out.
    TimedOut,
    /// The run is waiting on a required manual action.
    ActionRequired,
    /// The run failed during startup (before jobs ran).
    StartupFailure,
    /// The run was skipped.
    Skipped,
    /// A neutral conclusion (neither pass nor fail).
    Neutral,
    /// The run was stale.
    Stale,
}

impl Conclusion {
    /// Whether this conclusion counts as a passing run.
    pub fn is_success(&self) -> bool {
        matches!(self, Conclusion::Success)
    }

    /// A short, stable, lower-case label for this conclusion.
    pub fn label(&self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
            Conclusion::Cancelled => "cancelled",
            Conclusion::TimedOut => "timed_out",
            Conclusion::ActionRequired => "action_required",
            Conclusion::StartupFailure => "startup_failure",
            Conclusion::Skipped => "skipped",
            Conclusion::Neutral => "neutral",
            Conclusion::Stale => "stale",
        }
    }

    /// A single-character status glyph for human output.
    pub fn icon(&self) -> &'static str {
        match self {
            Conclusion::Success => "✔",
            Conclusion::Failure | Conclusion::StartupFailure => "✘",
            Conclusion::TimedOut => "⏱",
            Conclusion::Cancelled | Conclusion::Stale => "⊘",
            Conclusion::ActionRequired => "⚑",
            Conclusion::Skipped | Conclusion::Neutral => "•",
        }
    }
}

impl RunReport {
    /// Whether this run is passing.
    pub fn is_success(&self) -> bool {
        self.conclusion.is_success()
    }
}

impl RepoReport {
    /// Whether this repository has any non-successful run.
    pub fn has_failures(&self) -> bool {
        self.runs.iter().any(|r| !r.is_success())
    }
}

impl Report {
    /// Whether any inspected run across all repositories was non-successful.
    /// Drives the `--fail-on-failure` exit code.
    pub fn has_failures(&self) -> bool {
        self.repos.iter().any(RepoReport::has_failures)
    }

    /// Build a deterministic placeholder report (no network).
    ///
    /// It intentionally covers every rendering path: a passing run, a failing
    /// run with nested failed jobs/steps, a neutral "no runs" repository, and a
    /// per-repository error — so all three output formats can be exercised end
    /// to end in Phase 1.
    pub fn stub(limit: u32) -> Self {
        Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit,
            repos: vec![
                RepoReport {
                    repo: "bbugyi200/actstat".to_string(),
                    runs: vec![RunReport {
                        workflow: "CI".to_string(),
                        title: "Add list subcommand".to_string(),
                        run_number: 42,
                        event: "push".to_string(),
                        branch: "master".to_string(),
                        sha: "a1b2c3d".to_string(),
                        conclusion: Conclusion::Success,
                        url: "https://github.com/bbugyi200/actstat/actions/runs/1001".to_string(),
                        created_at: "2026-06-29T11:50:00Z".to_string(),
                        updated_at: "2026-06-29T11:52:30Z".to_string(),
                        jobs: vec![],
                    }],
                    error: None,
                },
                RepoReport {
                    repo: "bbugyi200/dotfiles".to_string(),
                    runs: vec![RunReport {
                        workflow: "CI".to_string(),
                        title: "Refactor shell init".to_string(),
                        run_number: 128,
                        event: "pull_request".to_string(),
                        branch: "feature/shell".to_string(),
                        sha: "9f8e7d6".to_string(),
                        conclusion: Conclusion::Failure,
                        url: "https://github.com/bbugyi200/dotfiles/actions/runs/2002".to_string(),
                        created_at: "2026-06-29T11:40:00Z".to_string(),
                        updated_at: "2026-06-29T11:44:10Z".to_string(),
                        jobs: vec![JobReport {
                            name: "test (3.14)".to_string(),
                            conclusion: Conclusion::Failure,
                            url: "https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003"
                                .to_string(),
                            steps: vec![StepReport {
                                name: "Run tests".to_string(),
                                number: 5,
                                conclusion: Conclusion::Failure,
                            }],
                        }],
                    }],
                    error: None,
                },
                RepoReport {
                    repo: "sase-org/example".to_string(),
                    runs: vec![],
                    error: None,
                },
                RepoReport {
                    repo: "bobs-org/locked".to_string(),
                    runs: vec![],
                    error: Some("403 Forbidden (token lacks access)".to_string()),
                },
            ],
        }
    }
}
