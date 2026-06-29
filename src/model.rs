//! The single normalized result model that every output format renders from.
//!
//! `Report -> [RepoReport] -> [CommitReport] -> [RunReport] -> [JobReport] -> [StepReport]`.
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
    /// The per-repository settled-commit limit that was requested
    /// (`-n/--limit`).
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
    /// The most-recent settled commits, newest first. Empty repos are
    /// suppressed before rendering unless they carry an error.
    pub commits: Vec<CommitReport>,
    /// A per-repository error (no access, Actions disabled, rate-limited, …).
    /// Present errors never abort the whole run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A settled commit on a repository's default branch, with all workflow runs
/// for that commit grouped below it.
#[derive(Debug, Clone, Serialize)]
pub struct CommitReport {
    /// Short (7-char) commit SHA.
    pub sha: String,
    /// Commit title / representative run display title.
    pub title: String,
    /// Default branch this commit was evaluated on.
    pub branch: String,
    /// Representative triggering event (`push`, `schedule`, …).
    pub event: String,
    /// Commit-level rollup: `success` when no run is an actionable problem,
    /// `failure` otherwise.
    pub conclusion: Conclusion,
    /// Direct URL to the commit on github.com.
    pub url: String,
    /// Latest `updated_at` among the commit's completed runs.
    pub finished_at: String,
    /// Aggregate wall-clock duration in seconds (earliest run start → latest
    /// run finish), omitted when any required timestamp is absent or malformed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// All completed workflow runs for this commit. Problem runs carry failed
    /// jobs/steps; non-problem runs have no jobs attached.
    pub runs: Vec<RunReport>,
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
    /// Wall-clock duration of the run in seconds (start → finish). `None` when
    /// the GitHub timestamps it is computed from are absent or malformed, so the
    /// field is simply omitted rather than reported as a misleading zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// Only problem jobs are attached; empty for non-problem runs.
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

/// Normalized workflow-run conclusion. Commit aggregation treats `success`,
/// `skipped`, and `neutral` as non-problem outcomes; every other variant is
/// rendered with its own label/icon.
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
    /// Whether this conclusion is literally GitHub `success`.
    pub fn is_success(&self) -> bool {
        matches!(self, Conclusion::Success)
    }

    /// Whether this conclusion is an actionable problem. Success, skipped, and
    /// neutral are non-problem outcomes; everything else turns a commit red.
    pub fn is_problem(&self) -> bool {
        !matches!(
            self,
            Conclusion::Success | Conclusion::Skipped | Conclusion::Neutral
        )
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

    /// Parse a GitHub `conclusion` string (from a run, job, or step) into a
    /// [`Conclusion`]. This is the inverse of [`Conclusion::label`].
    ///
    /// An unknown or absent (`null`) conclusion maps to [`Conclusion::Failure`]
    /// so anything unexpected is surfaced loudly rather than silently treated as
    /// a pass. In practice a `status=completed` run always carries a known
    /// conclusion; the fallback only guards against malformed payloads or a new
    /// conclusion GitHub adds in the future.
    pub fn from_github(value: Option<&str>) -> Conclusion {
        match value {
            Some("success") => Conclusion::Success,
            Some("failure") => Conclusion::Failure,
            Some("cancelled") => Conclusion::Cancelled,
            Some("timed_out") => Conclusion::TimedOut,
            Some("action_required") => Conclusion::ActionRequired,
            Some("startup_failure") => Conclusion::StartupFailure,
            Some("skipped") => Conclusion::Skipped,
            Some("neutral") => Conclusion::Neutral,
            Some("stale") => Conclusion::Stale,
            _ => Conclusion::Failure,
        }
    }
}

impl RunReport {
    /// Whether this run is an actionable problem.
    pub fn is_problem(&self) -> bool {
        self.conclusion.is_problem()
    }
}

impl CommitReport {
    /// Compute the aggregate commit conclusion from normalized runs.
    pub fn aggregate_conclusion(runs: &[RunReport]) -> Conclusion {
        if runs.iter().any(RunReport::is_problem) {
            Conclusion::Failure
        } else {
            Conclusion::Success
        }
    }

    /// Whether the commit-level rollup is green.
    pub fn is_success(&self) -> bool {
        self.conclusion.is_success()
    }
}

impl RepoReport {
    /// Whether this repository has any red commit.
    pub fn has_failures(&self) -> bool {
        self.commits.iter().any(|c| !c.is_success())
    }
}

impl Report {
    /// Whether any inspected commit across all repositories was red.
    /// Drives the `--fail-on-failure` exit code.
    pub fn has_failures(&self) -> bool {
        self.repos.iter().any(RepoReport::has_failures)
    }

    /// Build a deterministic placeholder report (no network).
    ///
    /// It intentionally covers every rendering path: a green commit, a red
    /// commit with a failed run plus a passing sibling run, and a per-repository
    /// error — so all three output formats can be exercised end to end without
    /// network access.
    pub fn stub(limit: u32) -> Self {
        Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit,
            repos: vec![
                RepoReport {
                    repo: "bbugyi200/actstat".to_string(),
                    commits: vec![CommitReport {
                        sha: "a1b2c3d".to_string(),
                        title: "Add list subcommand".to_string(),
                        branch: "master".to_string(),
                        event: "push".to_string(),
                        conclusion: Conclusion::Success,
                        url: "https://github.com/bbugyi200/actstat/commit/a1b2c3d4e5f67890"
                            .to_string(),
                        finished_at: "2026-06-29T11:52:30Z".to_string(),
                        // 11:50:00 → 11:52:30 = 150s.
                        duration_seconds: Some(150),
                        runs: vec![RunReport {
                            workflow: "CI".to_string(),
                            title: "Add list subcommand".to_string(),
                            run_number: 42,
                            event: "push".to_string(),
                            branch: "master".to_string(),
                            sha: "a1b2c3d".to_string(),
                            conclusion: Conclusion::Success,
                            url: "https://github.com/bbugyi200/actstat/actions/runs/1001"
                                .to_string(),
                            created_at: "2026-06-29T11:50:00Z".to_string(),
                            updated_at: "2026-06-29T11:52:30Z".to_string(),
                            duration_seconds: Some(150),
                            jobs: vec![],
                        }],
                    }],
                    error: None,
                },
                RepoReport {
                    repo: "bbugyi200/dotfiles".to_string(),
                    commits: vec![CommitReport {
                        sha: "9f8e7d6".to_string(),
                        title: "Refactor shell init".to_string(),
                        branch: "master".to_string(),
                        event: "push".to_string(),
                        conclusion: Conclusion::Failure,
                        url: "https://github.com/bbugyi200/dotfiles/commit/9f8e7d6c5b4a3210"
                            .to_string(),
                        finished_at: "2026-06-29T11:44:10Z".to_string(),
                        // 11:40:00 → 11:44:10 = 250s.
                        duration_seconds: Some(250),
                        runs: vec![
                            RunReport {
                                workflow: "CI".to_string(),
                                title: "Refactor shell init".to_string(),
                                run_number: 128,
                                event: "push".to_string(),
                                branch: "master".to_string(),
                                sha: "9f8e7d6".to_string(),
                                conclusion: Conclusion::Failure,
                                url: "https://github.com/bbugyi200/dotfiles/actions/runs/2002"
                                    .to_string(),
                                created_at: "2026-06-29T11:40:00Z".to_string(),
                                updated_at: "2026-06-29T11:44:10Z".to_string(),
                                duration_seconds: Some(250),
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
                            },
                            RunReport {
                                workflow: "Deploy Docs".to_string(),
                                title: "Refactor shell init".to_string(),
                                run_number: 33,
                                event: "push".to_string(),
                                branch: "master".to_string(),
                                sha: "9f8e7d6".to_string(),
                                conclusion: Conclusion::Success,
                                url: "https://github.com/bbugyi200/dotfiles/actions/runs/2003"
                                    .to_string(),
                                created_at: "2026-06-29T11:41:00Z".to_string(),
                                updated_at: "2026-06-29T11:43:00Z".to_string(),
                                duration_seconds: Some(120),
                                jobs: vec![],
                            },
                        ],
                    }],
                    error: None,
                },
                RepoReport {
                    repo: "bobs-org/locked".to_string(),
                    commits: vec![],
                    error: Some("403 Forbidden (token lacks access)".to_string()),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_github_round_trips_every_known_conclusion() {
        for c in [
            Conclusion::Success,
            Conclusion::Failure,
            Conclusion::Cancelled,
            Conclusion::TimedOut,
            Conclusion::ActionRequired,
            Conclusion::StartupFailure,
            Conclusion::Skipped,
            Conclusion::Neutral,
            Conclusion::Stale,
        ] {
            assert_eq!(
                Conclusion::from_github(Some(c.label())),
                c,
                "label() and from_github() must be inverses for {c:?}"
            );
        }
    }

    #[test]
    fn from_github_maps_unknown_and_missing_to_failure() {
        // Anything unrecognized or absent is treated as a (loud) failure rather
        // than silently passing.
        assert_eq!(
            Conclusion::from_github(Some("brand_new_thing")),
            Conclusion::Failure
        );
        assert_eq!(Conclusion::from_github(None), Conclusion::Failure);
        assert!(!Conclusion::from_github(None).is_success());
    }

    #[test]
    fn aggregate_conclusion_treats_skipped_and_neutral_as_green() {
        let mut run = RunReport {
            workflow: "CI".to_string(),
            title: "No-op".to_string(),
            run_number: 1,
            event: "push".to_string(),
            branch: "main".to_string(),
            sha: "abcdef1".to_string(),
            conclusion: Conclusion::Skipped,
            url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            duration_seconds: None,
            jobs: vec![],
        };
        assert_eq!(
            CommitReport::aggregate_conclusion(&[run.clone()]),
            Conclusion::Success
        );

        run.conclusion = Conclusion::Neutral;
        assert_eq!(
            CommitReport::aggregate_conclusion(&[run.clone()]),
            Conclusion::Success
        );

        run.conclusion = Conclusion::Failure;
        assert_eq!(
            CommitReport::aggregate_conclusion(&[run]),
            Conclusion::Failure
        );
    }

    #[test]
    fn report_has_failures_folds_over_commits() {
        let mut report = Report::stub(1);
        assert!(report.has_failures());
        report.repos.retain(|repo| repo.repo == "bbugyi200/actstat");
        assert!(!report.has_failures());
    }
}
