//! Output renderers. Every format reads the same normalized [`Report`] tree.
//!
//! - [`human`]: adaptive, colorized terminal output; reads cleanly with color
//!   stripped (`--color never` / `NO_COLOR`).
//! - [`json`]: one pretty-printed JSON document (metadata + repositories).
//! - [`jsonl`]: one record per line — one per commit, plus one per repo error —
//!   each carrying its `repo`, for easy `jq`/shell piping.
//!
//! All three return a `String` (with a trailing newline) so they are trivially
//! unit-testable; the binary just writes the result to stdout.

use std::fmt::Write as _;

use owo_colors::{OwoColorize, Style};

use crate::model::{
    ActiveCommitReport, ActiveRunReport, CommitReport, Conclusion, Report,
    RunReport, RunStatus,
};

/// Apply `style` to `text` only when `use_color` is set; otherwise return the
/// text verbatim with no escape codes (keeps no-color output byte-clean).
fn paint(use_color: bool, style: Style, text: &str) -> String {
    if use_color {
        text.style(style).to_string()
    } else {
        text.to_string()
    }
}

/// Pick a color for a conclusion's glyph/label.
fn conclusion_style(conclusion: Conclusion) -> Style {
    match conclusion {
        Conclusion::Success => Style::new().green(),
        Conclusion::Failure
        | Conclusion::StartupFailure
        | Conclusion::TimedOut => Style::new().red(),
        Conclusion::Cancelled
        | Conclusion::Stale
        | Conclusion::ActionRequired => Style::new().yellow(),
        Conclusion::Skipped | Conclusion::Neutral => Style::new().dimmed(),
    }
}

/// Pick a color for an active run status.
fn status_style(_: RunStatus) -> Style {
    Style::new().cyan()
}

/// Render the report as adaptive human-readable text.
///
/// `use_color` controls ANSI styling; `only_failures` suppresses green commits
/// (human output only — machine formats always include everything).
///
/// Output is grouped by repository (one block each, separated by a blank line):
/// a compact one-line summary per green commit, an expanded tree for red
/// commits, and clear single-line rows for errored repositories. Repositories
/// the filters hide entirely produce no block.
pub fn human(report: &Report, use_color: bool, only_failures: bool) -> String {
    let bold = Style::new().bold();
    let dim = Style::new().dimmed();
    let mut blocks: Vec<String> = Vec::new();

    for repo in &report.repos {
        let mut block = String::new();

        // Per-repository error: a clear, self-contained row (always shown — an
        // error is unhealthy, so --only-failures keeps it).
        if let Some(err) = &repo.error {
            let _ = writeln!(
                block,
                "{} {}  {}",
                paint(use_color, Style::new().red(), "✘"),
                paint(use_color, bold, &repo.repo),
                paint(use_color, Style::new().red(), err),
            );
            blocks.push(block);
            continue;
        }

        let visible_active: Vec<&ActiveCommitReport> =
            repo.active.iter().filter(|_| !only_failures).collect();
        let visible_commits: Vec<&CommitReport> = repo
            .commits
            .iter()
            .filter(|c| !only_failures || !c.is_success())
            .collect();
        if visible_active.is_empty() && visible_commits.is_empty() {
            continue;
        }

        let _ = writeln!(block, "{}", paint(use_color, bold, &repo.repo));
        for commit in visible_active {
            render_active_commit(
                &mut block,
                commit,
                &report.generated_at,
                use_color,
            );
        }
        for commit in visible_commits {
            render_commit(&mut block, commit, &report.generated_at, use_color);
        }
        blocks.push(block);
    }

    if blocks.is_empty() {
        return format!("{}\n", paint(use_color, dim, "no commits to report"));
    }
    // Each block already ends in a newline; joining with one more inserts a
    // single blank line between repositories without a trailing blank line.
    blocks.join("\n")
}

/// Render one active commit, always expanded to its running workflow run.
fn render_active_commit(
    out: &mut String,
    commit: &ActiveCommitReport,
    generated_at: &str,
    use_color: bool,
) {
    let style = status_style(RunStatus::InProgress);
    let dim = Style::new().dimmed();

    let mut meta: Vec<String> = Vec::new();
    if !commit.branch.is_empty() {
        meta.push(commit.branch.clone());
    }
    if commit.runs.len() > 1 {
        meta.push(format!("{} workflows", commit.runs.len()));
    }
    if let Some(elapsed) = elapsed_duration(&commit.started_at, generated_at) {
        meta.push(elapsed);
    }
    let meta = meta.join(" · ");
    let meta_prefix = if meta.is_empty() {
        "·".to_string()
    } else {
        format!("· {meta} ·")
    };

    let title = if commit.title.is_empty() {
        "(untitled commit)"
    } else {
        commit.title.as_str()
    };
    let summary = format!("{} {}", commit.sha, title);

    let _ = writeln!(
        out,
        "  {} {} {} {}",
        paint(use_color, style, RunStatus::InProgress.icon()),
        summary,
        paint(use_color, dim, &meta_prefix),
        paint(use_color, style, commit.rollup_label()),
    );

    for run in &commit.runs {
        render_active_run(out, run, generated_at, use_color);
    }
}

/// Render one active run under an active commit.
fn render_active_run(
    out: &mut String,
    run: &ActiveRunReport,
    generated_at: &str,
    use_color: bool,
) {
    let style = status_style(run.status);
    let dim = Style::new().dimmed();

    let mut meta = vec![format!("#{}", run.run_number)];
    if let Some(started_at) = run.started_at.as_deref() {
        if let Some(elapsed) = elapsed_duration(started_at, generated_at) {
            meta.push(elapsed);
        }
    }
    meta.push(run.status.label().to_string());
    let meta = meta.join(" · ");

    let workflow = if run.workflow.is_empty() {
        "(unknown workflow)"
    } else {
        run.workflow.as_str()
    };

    let _ = writeln!(
        out,
        "      {} {} {}",
        paint(use_color, style, run.status.icon()),
        workflow,
        paint(use_color, dim, &format!("· {meta}")),
    );

    if !run.url.is_empty() {
        let _ = writeln!(out, "          {}", paint(use_color, dim, &run.url));
    }
}

/// Render a single commit: a compact one-liner for green commits, plus an
/// expanded tree of failed runs → failed jobs → failed steps → URLs for red
/// commits.
///
/// `generated_at` is the report timestamp the relative "x ago" time is measured
/// against, so the rendering is deterministic for a given report.
fn render_commit(
    out: &mut String,
    commit: &CommitReport,
    generated_at: &str,
    use_color: bool,
) {
    let style = conclusion_style(commit.conclusion);
    let dim = Style::new().dimmed();

    // Compact metadata trailing the commit title: branch, workflow count when
    // useful, aggregate duration when known, and relative completion time.
    let mut meta: Vec<String> = Vec::new();
    if !commit.branch.is_empty() {
        meta.push(commit.branch.clone());
    }
    if commit.runs.len() > 1 {
        meta.push(format!("{} workflows", commit.runs.len()));
    }
    if let Some(secs) = commit.duration_seconds {
        meta.push(humanize_duration(secs));
    }
    if let Some(rel) = relative_time(&commit.finished_at, generated_at) {
        meta.push(rel);
    }
    let meta = meta.join(" · ");

    let title = if commit.title.is_empty() {
        "(untitled commit)"
    } else {
        commit.title.as_str()
    };
    let summary = format!("{} {}", commit.sha, title);

    if commit.is_success() {
        let _ = writeln!(
            out,
            "  {} {} {}",
            paint(use_color, style, commit.conclusion.icon()),
            summary,
            paint(use_color, dim, &format!("· {meta}")),
        );
        return;
    }

    let _ = writeln!(
        out,
        "  {} {} {} {}",
        paint(use_color, style, commit.conclusion.icon()),
        summary,
        paint(use_color, dim, &format!("· {meta} ·")),
        paint(use_color, style, commit.conclusion.label()),
    );

    for run in commit.runs.iter().filter(|r| r.is_problem()) {
        render_failed_run(out, run, use_color);
    }
}

/// Render one problem run inside a red commit.
fn render_failed_run(out: &mut String, run: &RunReport, use_color: bool) {
    let style = conclusion_style(run.conclusion);
    let dim = Style::new().dimmed();

    let mut meta = vec![format!("#{}", run.run_number)];
    if let Some(secs) = run.duration_seconds {
        meta.push(humanize_duration(secs));
    }
    meta.push(run.conclusion.label().to_string());
    let meta = meta.join(" · ");

    let workflow = if run.workflow.is_empty() {
        "(unknown workflow)"
    } else {
        run.workflow.as_str()
    };

    let _ = writeln!(
        out,
        "      {} {} {}",
        paint(use_color, style, run.conclusion.icon()),
        workflow,
        paint(use_color, dim, &format!("· {meta}")),
    );

    for job in &run.jobs {
        let _ = writeln!(
            out,
            "          {} {}",
            paint(
                use_color,
                conclusion_style(job.conclusion),
                job.conclusion.icon()
            ),
            job.name,
        );
        for step in &job.steps {
            let _ = writeln!(
                out,
                "              {}",
                paint(
                    use_color,
                    dim,
                    &format!("step {}: {}", step.number, step.name)
                ),
            );
        }
        if !job.url.is_empty() {
            let _ = writeln!(
                out,
                "              {}",
                paint(use_color, dim, &job.url)
            );
        }
    }
    // A final jump-to link for the run as a whole.
    if !run.url.is_empty() {
        let _ = writeln!(out, "          {}", paint(use_color, dim, &run.url));
    }
}

/// Render a compact relative "x ago" time for `then` measured against `now`,
/// both RFC3339 timestamps. Returns `None` (so the caller simply omits it) when
/// either timestamp is absent or unparseable.
fn relative_time(then: &str, now: &str) -> Option<String> {
    let then_ts: jiff::Timestamp = then.parse().ok()?;
    let now_ts: jiff::Timestamp = now.parse().ok()?;
    Some(humanize_ago(now_ts.as_second() - then_ts.as_second()))
}

/// Render elapsed time from `then` to `now` as a compact duration.
fn elapsed_duration(then: &str, now: &str) -> Option<String> {
    let then_ts: jiff::Timestamp = then.parse().ok()?;
    let now_ts: jiff::Timestamp = now.parse().ok()?;
    Some(humanize_duration(
        (now_ts.as_second() - then_ts.as_second()).max(0) as u64,
    ))
}

/// Format an elapsed number of seconds as a compact "x ago" string, picking the
/// largest sensible unit. Non-positive inputs (clock skew / future) read as
/// "just now".
fn humanize_ago(secs: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    if secs <= 0 {
        return "just now".to_string();
    }
    let (n, unit) = if secs < MIN {
        (secs, "s")
    } else if secs < HOUR {
        (secs / MIN, "m")
    } else if secs < DAY {
        (secs / HOUR, "h")
    } else if secs < WEEK {
        (secs / DAY, "d")
    } else if secs < MONTH {
        (secs / WEEK, "w")
    } else if secs < YEAR {
        (secs / MONTH, "mo")
    } else {
        (secs / YEAR, "y")
    };
    format!("{n}{unit} ago")
}

/// Format a run's wall-clock duration compactly, using the two largest sensible
/// units so it stays short: `45s`, `2m30s`, `1h5m`. Unlike [`humanize_ago`] this
/// carries no "ago" suffix, so a duration token reads distinctly from the
/// relative completion time it sits beside.
fn humanize_duration(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    if secs < MIN {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m{}s", secs / MIN, secs % MIN)
    } else {
        format!("{}h{}m", secs / HOUR, (secs % HOUR) / MIN)
    }
}

/// Render the report as a single pretty-printed JSON document.
pub fn json(report: &Report) -> String {
    let mut s = serde_json::to_string_pretty(report)
        .expect("Report always serializes to JSON");
    s.push('\n');
    s
}

/// Render the report as JSONL: one line per commit, plus one per repo error,
/// each tagged with `type` and `repo`.
pub fn jsonl(report: &Report) -> String {
    let mut out = String::new();
    for repo in &report.repos {
        if let Some(err) = &repo.error {
            let line = serde_json::json!({
                "type": "repo_error",
                "repo": repo.repo,
                "error": err,
            });
            let _ = writeln!(out, "{line}");
        }
        for commit in &repo.active {
            let mut value = serde_json::to_value(commit)
                .expect("ActiveCommitReport always serializes");
            if let serde_json::Value::Object(map) = &mut value {
                map.insert(
                    "type".to_string(),
                    serde_json::json!("active_commit"),
                );
                map.insert("repo".to_string(), serde_json::json!(repo.repo));
            }
            let _ = writeln!(out, "{value}");
        }
        for commit in &repo.commits {
            let mut value = serde_json::to_value(commit)
                .expect("CommitReport always serializes");
            // Tag each commit record with its origin so lines stand alone.
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("type".to_string(), serde_json::json!("commit"));
                map.insert("repo".to_string(), serde_json::json!(repo.repo));
            }
            let _ = writeln!(out, "{value}");
        }
    }
    out
}

/// Convenience: select and run the renderer for a [`Format`].
///
/// Kept here (rather than in `cli`) so renderer selection lives next to the
/// renderers themselves and is independently testable.
pub fn render(
    report: &Report,
    format: Format,
    use_color: bool,
    only_failures: bool,
) -> String {
    match format {
        Format::Human => human(report, use_color, only_failures),
        Format::Json => json(report),
        Format::Jsonl => jsonl(report),
    }
}

pub use crate::cli::Format;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RepoReport, Report};

    #[test]
    fn json_is_a_single_valid_document() {
        let report = Report::stub(1);
        let out = json(&report);
        let value: serde_json::Value =
            serde_json::from_str(&out).expect("json renders valid JSON");
        assert!(value.get("repositories").is_some());
        assert_eq!(value["limit"], 1);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn jsonl_has_one_record_per_commit_plus_per_error() {
        let report = Report::stub(1);
        let expected_commits: usize =
            report.repos.iter().map(|r| r.commits.len()).sum();
        let expected_active: usize =
            report.repos.iter().map(|r| r.active.len()).sum();
        let expected_errors =
            report.repos.iter().filter(|r| r.error.is_some()).count();
        let out = jsonl(&report);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            expected_active + expected_commits + expected_errors
        );

        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)
                .expect("each jsonl line is valid JSON");
            let kind = value["type"].as_str().expect("each record is tagged");
            assert!(
                kind == "commit"
                    || kind == "active_commit"
                    || kind == "repo_error"
            );
            assert!(
                value.get("repo").is_some(),
                "each record carries its repo"
            );
        }
    }

    #[test]
    fn human_no_color_has_no_escape_codes() {
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(
            !out.contains('\u{1b}'),
            "no ANSI escape codes without color"
        );
        assert!(out.contains("bbugyi200/actstat"));
        assert!(out.contains("403 Forbidden"), "repo error is shown");
    }

    #[test]
    fn human_suppresses_empty_repo_blocks_but_keeps_errors() {
        let report = Report {
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            limit: 1,
            repos: vec![
                RepoReport {
                    repo: "o/empty".to_string(),
                    active: vec![],
                    commits: vec![],
                    error: None,
                },
                RepoReport {
                    repo: "o/locked".to_string(),
                    active: vec![],
                    commits: vec![],
                    error: Some("403 Forbidden".to_string()),
                },
            ],
        };

        let out = human(&report, false, false);

        assert!(!out.contains("o/empty"));
        assert!(out.contains("o/locked"));
        assert!(out.contains("403 Forbidden"));
    }

    #[test]
    fn human_expands_failed_runs_with_jobs_and_steps() {
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(out.contains("test (3.14)"), "failed job is shown");
        assert!(out.contains("Run tests"), "failed step is shown");
    }

    #[test]
    fn human_with_color_emits_escape_codes() {
        let report = Report::stub(1);
        let out = human(&report, true, false);
        assert!(out.contains('\u{1b}'), "color output contains ANSI codes");
    }

    #[test]
    fn only_failures_hides_green_commits_in_human() {
        let report = Report::stub(1);
        let out = human(&report, false, true);
        // The passing-only repo's whole block (header included) is gone; the
        // failing repo and its failed run stay, as does the errored repo.
        assert!(
            !out.contains("bbugyi200/actstat"),
            "a repo with only green commits is hidden"
        );
        assert!(out.contains("bbugyi200/dotfiles"), "failing repo stays");
        assert!(out.contains("#128"), "the failing run stays");
        assert!(out.contains("bobs-org/locked"), "errors are still shown");
    }

    #[test]
    fn human_commit_line_shows_relative_time() {
        // Stub `generated_at` is 12:00:00Z; the green commit finished at
        // 11:52:30Z (7m30s earlier) and the red one at 11:44:10Z (15m50s).
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(out.contains("7m ago"), "green commit shows relative time");
        assert!(out.contains("15m ago"), "red commit shows relative time");
    }

    #[test]
    fn human_failure_line_carries_conclusion_label() {
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(
            out.contains("· failure"),
            "red commits label the aggregate conclusion explicitly"
        );
    }

    #[test]
    fn human_separates_repositories_with_a_blank_line() {
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(
            out.contains("\n\n"),
            "repository blocks are visually separated"
        );
        assert!(!out.ends_with("\n\n"), "but no trailing blank line");
    }

    #[test]
    fn human_no_color_snapshot_is_stable() {
        // A full, deterministic snapshot of the no-color human layout. If the
        // format intentionally changes, update this expected block.
        let report = Report::stub(1);
        let expected = "\
bbugyi200/actstat
  ↻ f00ba12 Add progress spinner · master · 1m20s · running
      ↻ CI · #44 · 1m20s · in_progress
          https://github.com/bbugyi200/actstat/actions/runs/1044
  ✔ a1b2c3d Add list subcommand · master · 2m30s · 7m ago

bbugyi200/dotfiles
  ✘ 9f8e7d6 Refactor shell init · master · 2 workflows · 4m10s · 15m ago · failure
      ✘ CI · #128 · 4m10s · failure
          ✘ test (3.14)
              step 5: Run tests
              https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003
          https://github.com/bbugyi200/dotfiles/actions/runs/2002

✘ bobs-org/locked  403 Forbidden (token lacks access)
";
        assert_eq!(human(&report, false, false), expected);
    }

    #[test]
    fn humanize_ago_picks_the_largest_sensible_unit() {
        assert_eq!(humanize_ago(0), "just now");
        assert_eq!(humanize_ago(-5), "just now");
        assert_eq!(humanize_ago(30), "30s ago");
        assert_eq!(humanize_ago(90), "1m ago");
        assert_eq!(humanize_ago(3 * 3600), "3h ago");
        assert_eq!(humanize_ago(2 * 86400), "2d ago");
        assert_eq!(humanize_ago(10 * 86400), "1w ago");
        assert_eq!(humanize_ago(45 * 86400), "1mo ago");
        assert_eq!(humanize_ago(400 * 86400), "1y ago");
    }

    #[test]
    fn humanize_duration_uses_two_compact_units() {
        assert_eq!(humanize_duration(0), "0s");
        assert_eq!(humanize_duration(45), "45s");
        assert_eq!(humanize_duration(150), "2m30s");
        assert_eq!(humanize_duration(250), "4m10s");
        assert_eq!(humanize_duration(120), "2m0s");
        assert_eq!(humanize_duration(3905), "1h5m");
    }

    #[test]
    fn human_commit_line_shows_duration() {
        // Stub commit durations: actstat 150s (2m30s), dotfiles 250s (4m10s).
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(out.contains("2m30s"), "green commit shows its duration");
        assert!(out.contains("4m10s"), "red commit shows its duration");
    }

    #[test]
    fn relative_time_is_none_for_unparseable_input() {
        assert_eq!(relative_time("", "2026-06-29T12:00:00Z"), None);
        assert_eq!(relative_time("2026-06-29T12:00:00Z", "nonsense"), None);
        assert_eq!(
            relative_time("2026-06-29T11:00:00Z", "2026-06-29T12:00:00Z")
                .as_deref(),
            Some("1h ago")
        );
    }

    #[test]
    fn elapsed_duration_is_none_for_unparseable_input() {
        assert_eq!(elapsed_duration("", "2026-06-29T12:00:00Z"), None);
        assert_eq!(
            elapsed_duration("2026-06-29T11:58:40Z", "2026-06-29T12:00:00Z")
                .as_deref(),
            Some("1m20s")
        );
    }

    #[test]
    fn json_always_carries_active_array() {
        let report = Report::stub(1);
        let value: serde_json::Value =
            serde_json::from_str(&json(&report)).unwrap();
        for repo in value["repositories"].as_array().unwrap() {
            assert!(repo["active"].is_array());
        }
    }

    #[test]
    fn jsonl_emits_active_commit_before_settled_commits() {
        let report = Report::stub(1);
        let out = jsonl(&report);
        let records: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let actstat_records: Vec<&serde_json::Value> = records
            .iter()
            .filter(|record| record["repo"] == "bbugyi200/actstat")
            .collect();
        assert_eq!(actstat_records[0]["type"], "active_commit");
        assert_eq!(actstat_records[1]["type"], "commit");
    }

    #[test]
    fn json_is_deterministic_for_the_same_report() {
        let report = Report::stub(3);
        assert_eq!(json(&report), json(&report), "JSON output is stable");
    }

    #[test]
    fn render_dispatches_to_each_format() {
        let report = Report::stub(2);
        let human_out = render(&report, Format::Human, false, false);
        let json_out = render(&report, Format::Json, false, false);
        let jsonl_out = render(&report, Format::Jsonl, false, false);

        assert_eq!(human_out, human(&report, false, false));
        assert_eq!(json_out, json(&report));
        assert_eq!(jsonl_out, jsonl(&report));
        // json is a single document; jsonl is multiple lines.
        assert!(serde_json::from_str::<serde_json::Value>(&json_out).is_ok());
        assert!(jsonl_out.lines().count() > 1);
    }
}
