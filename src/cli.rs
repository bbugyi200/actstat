//! Command-line contract for `actstat`.
//!
//! `actstat` exposes a single `list` subcommand and treats a bare invocation
//! (`actstat`) as `actstat list`. To make `actstat -n 3` an alias for
//! `actstat list -n 3`, the `list` arguments are flattened at the top level
//! alongside an `Option<Commands>` and clap's `args_conflicts_with_subcommands`
//! is enabled — so you use either the top-level args *or* the subcommand, and
//! both resolve to the same [`ListArgs`].

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{self, RepoName};
use crate::github::{self, DiscoveredToken};
use crate::model::{RepoReport, Report};
use crate::render;

/// Report the status of recent settled GitHub Actions commits across repos.
#[derive(Debug, Parser)]
#[command(
    name = "actstat",
    version,
    about,
    long_about = None,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// `list` arguments, available at the top level so `actstat -n 3` works.
    #[command(flatten)]
    pub list: ListArgs,

    /// Optional explicit subcommand. When omitted, behaves like `list`.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// The set of subcommands. Today there is exactly one.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// List recent settled commit status per repository (the default command).
    List(ListArgs),
}

/// Options for the `list` command (and the bare `actstat` invocation).
#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// Number of most-recent settled commits to inspect per repository.
    #[arg(
        short = 'n',
        long,
        value_name = "N",
        default_value_t = 1,
        value_parser = parse_limit
    )]
    pub limit: u32,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Human, value_name = "FORMAT")]
    pub format: Format,

    /// Explicit config path (overrides discovery).
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Color control. Also honors the `NO_COLOR` environment variable.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, value_name = "WHEN")]
    pub color: ColorChoice,

    /// Show only failing commits in human output.
    #[arg(long)]
    pub only_failures: bool,

    /// Skip fetching and showing the currently running workflow run.
    #[arg(long)]
    pub no_active: bool,

    /// Restrict this run to a subset of configured repositories (repeatable).
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    pub repos: Vec<String>,

    /// Max concurrent repositories in flight.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub concurrency: usize,

    /// Exit non-zero if any inspected commit is failing (for cron/CI).
    #[arg(long)]
    pub fail_on_failure: bool,

    /// Increase diagnostic verbosity (stderr only). Repeatable.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-error diagnostics (stderr only).
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,
}

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Adaptive, colorized terminal output (default).
    Human,
    /// A single JSON document.
    Json,
    /// One JSON record per line.
    Jsonl,
}

/// When to emit ANSI color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Color when stdout is a TTY and `NO_COLOR` is unset.
    Auto,
    /// Always emit color.
    Always,
    /// Never emit color.
    Never,
}

/// Parse `-n/--limit`, rejecting `0` with an actionable message.
fn parse_limit(s: &str) -> Result<u32, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a valid whole number"))?;
    if n == 0 {
        return Err("must be at least 1".to_string());
    }
    Ok(n)
}

impl Cli {
    /// Resolve the effective `list` arguments from either the flattened
    /// top-level args or the explicit `list` subcommand. This is the single
    /// place that makes `actstat`, `actstat list`, and `actstat -n 3` converge.
    pub fn list_args(self) -> ListArgs {
        match self.command {
            Some(Commands::List(args)) => args,
            None => self.list,
        }
    }
}

/// Decide whether to emit color, honoring `--color` and `NO_COLOR`.
pub fn use_color(choice: ColorChoice) -> bool {
    use std::io::IsTerminal as _;
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none()
                && std::io::stdout().is_terminal()
        }
    }
}

/// Parse the process arguments and run, returning the process exit code.
///
/// Collection is I/O-bound fan-out over many repositories, so this drives an
/// async `run_list` operation on a Tokio runtime. A runtime that fails to start
/// is an operational error (exit `1`).
pub fn run() -> ExitCode {
    let args = Cli::parse().list_args();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "actstat: failed to start async runtime: {e}"
            );
            return ExitCode::from(1);
        }
    };
    runtime.block_on(run_list(args))
}

/// Execute the `list` command end to end: load config, resolve repositories,
/// collect recent settled commit status, render the chosen format to stdout, and
/// return the exit code per the documented table.
///
/// Diagnostics, warnings, and progress go to stderr so machine output on stdout
/// stays pipe-clean; per-repository and per-org failures become inline rows
/// rather than aborting the run.
async fn run_list(args: ListArgs) -> ExitCode {
    // 1. Configuration — a missing/invalid config is an operational error.
    let config = match config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(e) => return config_error_exit(&e),
    };

    // 2. Token discovery — warn (but proceed) when unauthenticated.
    let token = github::discover_token();
    if !args.quiet {
        if let Some(warning) = token.unauthenticated_warning() {
            let _ = writeln!(std::io::stderr(), "actstat: warning: {warning}");
        }
    }
    if args.verbose > 0 && !args.quiet {
        diag_token_source(&token);
    }

    // 3. GitHub client — a client that cannot be built is fatal.
    let client = match github::GitHubClient::github(token.token) {
        Ok(client) => client,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "actstat: {e}");
            return ExitCode::from(1);
        }
    };

    // 4. Expand orgs + merge explicit repos into a sorted, de-duplicated list.
    let resolved =
        github::resolve_repositories(&client, &config, args.concurrency).await;

    // 5. Honor --repo (a malformed value is a usage error, exit 2).
    let repos = match filter_repos(resolved.repos, &args.repos) {
        Ok(repos) => repos,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "actstat: {e}");
            return ExitCode::from(2);
        }
    };
    if args.verbose > 0 && !args.quiet {
        let _ = writeln!(
            std::io::stderr(),
            "actstat: inspecting {} repositor{} (settled-commit limit={}, running-run lookup={}, concurrency={})",
            repos.len(),
            if repos.len() == 1 { "y" } else { "ies" },
            args.limit,
            if args.no_active { "skipped" } else { "included" },
            args.concurrency,
        );
    }

    // 6. Fan out collection, then fold any org-expansion failures in as rows.
    let mut reports = github::collect_repo_reports(
        &client,
        repos,
        args.limit,
        args.concurrency,
        !args.no_active,
    )
    .await;
    reports.extend(org_error_reports(resolved.org_errors));
    reports.sort_by(|a, b| a.repo.cmp(&b.repo));
    suppress_empty_success_reports(&mut reports);

    // 7. The single report every renderer reads from.
    let report = Report {
        generated_at: now_rfc3339(),
        limit: args.limit,
        repos: reports,
    };

    // 8. Render to stdout.
    let rendered = render::render(
        &report,
        args.format,
        use_color(args.color),
        args.only_failures,
    );
    print!("{rendered}");

    // 9. Exit code per the documented table.
    list_exit_code(&report, args.fail_on_failure)
}

/// Print a config error to stderr and return the operational-error exit code.
/// A "no config found" error is paired with a minimal example to get going.
fn config_error_exit(err: &config::ConfigError) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "actstat: {err}");
    if matches!(err, config::ConfigError::NotFound(_)) {
        let _ = writeln!(
            std::io::stderr(),
            "\nexample config:\n\n{}",
            config::example_config()
        );
    }
    ExitCode::from(1)
}

/// Emit a stderr diagnostic naming where the token came from (verbose only).
fn diag_token_source(token: &DiscoveredToken) {
    use github::TokenSource;
    let source = match &token.source {
        TokenSource::Env(name) => format!("environment variable {name}"),
        TokenSource::GhCli => "`gh auth token`".to_string(),
        TokenSource::Unauthenticated => "none (unauthenticated)".to_string(),
    };
    let _ = writeln!(std::io::stderr(), "actstat: token source: {source}");
}

/// Turn org-expansion failures into inline error rows so a single failed org
/// surfaces in the report instead of aborting the run.
fn org_error_reports(org_errors: Vec<(String, String)>) -> Vec<RepoReport> {
    org_errors
        .into_iter()
        .map(|(org, message)| RepoReport {
            repo: org,
            active: vec![],
            commits: vec![],
            error: Some(format!("failed to expand org: {message}")),
        })
        .collect()
}

/// Drop repositories that produced no settled commits and no error. Error rows
/// stay visible; empty successful repos are suppressed in every output format.
fn suppress_empty_success_reports(reports: &mut Vec<RepoReport>) {
    reports.retain(|report| {
        !report.active.is_empty()
            || !report.commits.is_empty()
            || report.error.is_some()
    });
}

/// Restrict `repos` to the `--repo` selection, if any. With no selection the
/// full resolved set is returned unchanged. Requested entries are validated as
/// `owner/name` (a malformed one is an error) and matched against the resolved
/// set, so an entry that isn't configured is simply dropped.
fn filter_repos(
    repos: Vec<RepoName>,
    requested: &[String],
) -> Result<Vec<RepoName>, String> {
    if requested.is_empty() {
        return Ok(repos);
    }
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for raw in requested {
        wanted.insert(RepoName::parse(raw)?.full_name());
    }
    Ok(repos
        .into_iter()
        .filter(|r| wanted.contains(&r.full_name()))
        .collect())
}

/// The current time as an RFC3339 timestamp for [`Report::generated_at`].
fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

/// Whether every inspected repository produced an error (and there was at least
/// one). This is an operational failure — distinct from a normal run where some
/// repositories are simply red.
fn all_repos_errored(report: &Report) -> bool {
    !report.repos.is_empty() && report.repos.iter().all(|r| r.error.is_some())
}

/// Map a rendered report to its process exit code per the documented table:
///
/// - `1` — operational failure (here: every inspected repository errored).
/// - `2` — `--fail-on-failure` is set and at least one inspected commit is red.
/// - `0` — otherwise; the command completed and conclusions were reported.
fn list_exit_code(report: &Report, fail_on_failure: bool) -> ExitCode {
    if all_repos_errored(report) {
        ExitCode::from(1)
    } else if fail_on_failure && report.has_failures() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse argv without touching the real process args.
    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    #[test]
    fn bare_invocation_dispatches_to_list_defaults() {
        let cli = parse(&["actstat"]).expect("bare invocation parses");
        let args = cli.list_args();
        assert_eq!(args.limit, 1);
        assert_eq!(args.format, Format::Human);
        assert_eq!(args.color, ColorChoice::Auto);
        assert!(!args.no_active);
    }

    #[test]
    fn explicit_list_subcommand_parses() {
        let cli = parse(&["actstat", "list"]).expect("`list` parses");
        assert!(matches!(cli.command, Some(Commands::List(_))));
        assert_eq!(cli.list_args().limit, 1);
    }

    #[test]
    fn default_command_equivalence_for_limit() {
        // `actstat -n 3`, `actstat list -n 3` must reach the same args.
        let bare = parse(&["actstat", "-n", "3"]).unwrap().list_args();
        let sub = parse(&["actstat", "list", "-n", "3"]).unwrap().list_args();
        assert_eq!(bare.limit, 3);
        assert_eq!(sub.limit, 3);
        assert_eq!(bare.limit, sub.limit);
    }

    #[test]
    fn long_limit_flag_parses() {
        assert_eq!(
            parse(&["actstat", "--limit", "5"])
                .unwrap()
                .list_args()
                .limit,
            5
        );
    }

    #[test]
    fn limit_zero_is_rejected() {
        let err = parse(&["actstat", "-n", "0"])
            .expect_err("limit 0 must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn limit_zero_rejected_under_subcommand_too() {
        let err = parse(&["actstat", "list", "-n", "0"])
            .expect_err("limit 0 must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn non_numeric_limit_is_rejected() {
        let err = parse(&["actstat", "-n", "abc"])
            .expect_err("non-numeric limit rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn bad_format_is_rejected() {
        let err = parse(&["actstat", "-f", "xml"])
            .expect_err("unknown format rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn each_format_value_parses() {
        for (text, expected) in [
            ("human", Format::Human),
            ("json", Format::Json),
            ("jsonl", Format::Jsonl),
        ] {
            let args = parse(&["actstat", "-f", text]).unwrap().list_args();
            assert_eq!(args.format, expected);
        }
    }

    #[test]
    fn color_values_parse() {
        for (text, expected) in [
            ("auto", ColorChoice::Auto),
            ("always", ColorChoice::Always),
            ("never", ColorChoice::Never),
        ] {
            let args =
                parse(&["actstat", "--color", text]).unwrap().list_args();
            assert_eq!(args.color, expected);
        }
    }

    #[test]
    fn repeated_repo_flag_collects() {
        let args = parse(&["actstat", "--repo", "a/b", "--repo", "c/d"])
            .unwrap()
            .list_args();
        assert_eq!(args.repos, vec!["a/b".to_string(), "c/d".to_string()]);
    }

    #[test]
    fn flags_parse_together() {
        let args = parse(&[
            "actstat",
            "list",
            "--only-failures",
            "--no-active",
            "--fail-on-failure",
            "--concurrency",
            "4",
            "-vv",
        ])
        .unwrap()
        .list_args();
        assert!(args.only_failures);
        assert!(args.no_active);
        assert!(args.fail_on_failure);
        assert_eq!(args.concurrency, 4);
        assert_eq!(args.verbose, 2);
    }

    #[test]
    fn no_active_parses_bare_and_under_list() {
        assert!(
            parse(&["actstat", "--no-active"])
                .unwrap()
                .list_args()
                .no_active
        );
        assert!(
            parse(&["actstat", "list", "--no-active"])
                .unwrap()
                .list_args()
                .no_active
        );
    }

    #[test]
    fn verbose_and_quiet_conflict() {
        let err = parse(&["actstat", "-v", "-q"])
            .expect_err("verbose+quiet must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn use_color_honors_choice() {
        assert!(!use_color(ColorChoice::Never));
        assert!(use_color(ColorChoice::Always));
    }

    // --- Exit-code policy (pure) ----------------------------------------

    /// Compare two `ExitCode`s by their debug rendering (they aren't `PartialEq`).
    fn same_code(a: ExitCode, b: ExitCode) -> bool {
        format!("{a:?}") == format!("{b:?}")
    }

    #[test]
    fn exit_two_when_fail_on_failure_and_a_commit_failed() {
        // The stub report mixes a green commit, a red commit, and an error row.
        let report = Report::stub(1);
        assert!(report.has_failures());
        assert!(!all_repos_errored(&report), "the stub is not all-errored");
        assert!(same_code(list_exit_code(&report, true), ExitCode::from(2)));
    }

    #[test]
    fn exit_zero_without_fail_on_failure_even_with_red_commits() {
        let report = Report::stub(1);
        assert!(same_code(list_exit_code(&report, false), ExitCode::SUCCESS));
    }

    #[test]
    fn exit_one_when_every_repository_errored() {
        let mut report = Report::stub(1);
        report.repos.retain(|r| r.error.is_some());
        assert!(all_repos_errored(&report));
        // Exit 1 wins over --fail-on-failure.
        assert!(same_code(list_exit_code(&report, true), ExitCode::from(1)));
    }

    #[test]
    fn exit_zero_for_an_empty_report() {
        let report = Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit: 1,
            repos: vec![],
        };
        assert!(!all_repos_errored(&report), "no repos is not 'all errored'");
        assert!(same_code(list_exit_code(&report, true), ExitCode::SUCCESS));
    }

    #[test]
    fn active_only_report_does_not_trip_fail_on_failure() {
        let active = Report::stub(1).repos[0].active.clone();
        let report = Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit: 1,
            repos: vec![RepoReport {
                repo: "o/r".to_string(),
                active,
                commits: vec![],
                error: None,
            }],
        };

        assert!(!report.has_failures());
        assert!(!all_repos_errored(&report));
        assert!(same_code(list_exit_code(&report, true), ExitCode::SUCCESS));
    }

    // --- --repo filtering (pure) ----------------------------------------

    fn repo(owner: &str, name: &str) -> RepoName {
        RepoName::parse(&format!("{owner}/{name}")).unwrap()
    }

    #[test]
    fn filter_repos_without_selection_returns_everything() {
        let repos = vec![repo("o", "a"), repo("o", "b")];
        let out = filter_repos(repos.clone(), &[]).unwrap();
        assert_eq!(out, repos);
    }

    #[test]
    fn filter_repos_keeps_only_the_selected_and_drops_unconfigured() {
        let repos = vec![repo("o", "a"), repo("o", "b"), repo("o", "c")];
        let out = filter_repos(
            repos,
            &[
                "o/b".to_string(),
                "o/c".to_string(),
                "o/not-configured".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(out, vec![repo("o", "b"), repo("o", "c")]);
    }

    #[test]
    fn filter_repos_rejects_a_malformed_selection() {
        let err = filter_repos(vec![repo("o", "a")], &["bogus".to_string()])
            .unwrap_err();
        assert!(err.contains("owner/name"), "got: {err}");
    }

    // --- org error rows -------------------------------------------------

    #[test]
    fn org_errors_become_inline_error_rows() {
        let rows = org_error_reports(vec![(
            "bad-org".to_string(),
            "not found (404): Not Found".to_string(),
        )]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo, "bad-org");
        assert!(rows[0].commits.is_empty());
        assert!(rows[0]
            .error
            .as_deref()
            .unwrap()
            .contains("failed to expand org"));
    }

    #[test]
    fn suppress_empty_reports_keeps_commits_and_errors() {
        let mut rows = Report::stub(1).repos;
        rows.push(RepoReport {
            repo: "o/empty".to_string(),
            active: vec![],
            commits: vec![],
            error: None,
        });

        suppress_empty_success_reports(&mut rows);

        assert!(rows.iter().any(|r| r.repo == "bbugyi200/actstat"));
        assert!(rows.iter().any(|r| r.repo == "bobs-org/locked"));
        assert!(!rows.iter().any(|r| r.repo == "o/empty"));
    }

    #[test]
    fn suppress_empty_reports_keeps_active_only_repos() {
        let active = Report::stub(1).repos[0].active.clone();
        let mut rows = vec![RepoReport {
            repo: "o/active".to_string(),
            active,
            commits: vec![],
            error: None,
        }];

        suppress_empty_success_reports(&mut rows);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo, "o/active");
    }
}
