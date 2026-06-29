//! Command-line contract for `actstat`.
//!
//! `actstat` exposes a single `list` subcommand and treats a bare invocation
//! (`actstat`) as `actstat list`. To make `actstat -n 3` an alias for
//! `actstat list -n 3`, the `list` arguments are flattened at the top level
//! alongside an `Option<Commands>` and clap's `args_conflicts_with_subcommands`
//! is enabled — so you use either the top-level args *or* the subcommand, and
//! both resolve to the same [`ListArgs`].

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::model::Report;
use crate::render;

/// Report the status of recent GitHub Actions workflow runs across repos.
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
    /// List recent workflow-run status per repository (the default command).
    List(ListArgs),
}

/// Options for the `list` command (and the bare `actstat` invocation).
#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// Number of most-recent completed runs to inspect per repository.
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

    /// Show only non-passing runs in human output.
    #[arg(long)]
    pub only_failures: bool,

    /// Restrict this run to a subset of configured repositories (repeatable).
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    pub repos: Vec<String>,

    /// Max concurrent repositories in flight.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub concurrency: usize,

    /// Exit non-zero if any inspected run is non-successful (for cron/CI).
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
pub fn run() -> ExitCode {
    let args = Cli::parse().list_args();
    run_list(&args)
}

/// Execute the `list` command. Phase 1 uses a network-free stub [`Report`] so
/// every format can be exercised end-to-end; later phases swap in real data.
pub fn run_list(args: &ListArgs) -> ExitCode {
    if args.verbose > 0 && !args.quiet {
        // Diagnostics go to stderr so stdout stays pipe-clean.
        let _ = writeln!(
            std::io::stderr(),
            "actstat: limit={} format={:?} repos={:?} (stub data; no network in phase 1)",
            args.limit,
            args.format,
            args.repos,
        );
    }

    let report = Report::stub(args.limit);
    let rendered = render::render(
        &report,
        args.format,
        use_color(args.color),
        args.only_failures,
    );

    // Machine + human output both go to stdout.
    print!("{rendered}");

    if args.fail_on_failure && report.has_failures() {
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
            "--fail-on-failure",
            "--concurrency",
            "4",
            "-vv",
        ])
        .unwrap()
        .list_args();
        assert!(args.only_failures);
        assert!(args.fail_on_failure);
        assert_eq!(args.concurrency, 4);
        assert_eq!(args.verbose, 2);
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

    #[test]
    fn fail_on_failure_yields_exit_two_on_failures() {
        // The stub report contains a failing run.
        let args = parse(&["actstat", "--fail-on-failure", "--color", "never"])
            .unwrap()
            .list_args();
        let code = run_list(&args);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
    }
}
