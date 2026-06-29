//! Output renderers. Every format reads the same normalized [`Report`] tree.
//!
//! - [`human`]: adaptive, colorized terminal output; reads cleanly with color
//!   stripped (`--color never` / `NO_COLOR`).
//! - [`json`]: one pretty-printed JSON document (metadata + repositories).
//! - [`jsonl`]: one record per line — one per run, plus one per repo error —
//!   each carrying its `repo`, for easy `jq`/shell piping.
//!
//! All three return a `String` (with a trailing newline) so they are trivially
//! unit-testable; the binary just writes the result to stdout.

use std::fmt::Write as _;

use owo_colors::{OwoColorize, Style};

use crate::model::{Conclusion, Report, RunReport};

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

/// Render the report as adaptive human-readable text.
///
/// `use_color` controls ANSI styling; `only_failures` suppresses passing runs
/// (human output only — machine formats always include everything).
///
/// Output is grouped by repository (one block each, separated by a blank line):
/// a compact one-line summary per passing run, an expanded tree for failures,
/// and clear single-line rows for neutral ("no completed runs") and errored
/// repositories. Repositories the filters hide entirely produce no block.
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

        // Neutral: no completed runs (never silently disappears). Hidden when
        // --only-failures is set, since there is nothing unhealthy to show.
        if repo.runs.is_empty() {
            if !only_failures {
                let _ = writeln!(
                    block,
                    "{} {}  {}",
                    paint(use_color, dim, "•"),
                    paint(use_color, bold, &repo.repo),
                    paint(use_color, dim, "no completed runs"),
                );
                blocks.push(block);
            }
            continue;
        }

        let visible_runs: Vec<&RunReport> = repo
            .runs
            .iter()
            .filter(|r| !only_failures || !r.is_success())
            .collect();
        if visible_runs.is_empty() {
            continue;
        }

        let _ = writeln!(block, "{}", paint(use_color, bold, &repo.repo));
        for run in visible_runs {
            render_run(&mut block, run, &report.generated_at, use_color);
        }
        blocks.push(block);
    }

    if blocks.is_empty() {
        return format!("{}\n", paint(use_color, dim, "no runs to report"));
    }
    // Each block already ends in a newline; joining with one more inserts a
    // single blank line between repositories without a trailing blank line.
    blocks.join("\n")
}

/// Render a single run: a compact one-liner for passes, plus an expanded tree
/// of failed jobs → failed steps → URLs for non-successful runs.
///
/// `generated_at` is the report timestamp the relative "x ago" time is measured
/// against, so the rendering is deterministic for a given report.
fn render_run(
    out: &mut String,
    run: &RunReport,
    generated_at: &str,
    use_color: bool,
) {
    let style = conclusion_style(run.conclusion);
    let dim = Style::new().dimmed();

    // Compact metadata trailing the workflow name: branch (when known), run
    // number, and relative completion time.
    let mut meta: Vec<String> = Vec::new();
    if !run.branch.is_empty() {
        meta.push(run.branch.clone());
    }
    meta.push(format!("#{}", run.run_number));
    if let Some(rel) = relative_time(&run.updated_at, generated_at) {
        meta.push(rel);
    }
    let meta = meta.join(" · ");

    // A malformed payload can leave the workflow name empty; show a placeholder
    // rather than a confusing blank.
    let workflow = if run.workflow.is_empty() {
        "(unknown workflow)"
    } else {
        run.workflow.as_str()
    };

    if run.is_success() {
        let _ = writeln!(
            out,
            "  {} {} {}",
            paint(use_color, style, run.conclusion.icon()),
            workflow,
            paint(use_color, dim, &format!("· {meta}")),
        );
        return;
    }

    // Non-success: the same compact line plus an explicit conclusion label
    // (so cancelled/timed_out/etc. read clearly even with no failure detail),
    // then the expanded failure tree.
    let _ = writeln!(
        out,
        "  {} {} {} {}",
        paint(use_color, style, run.conclusion.icon()),
        workflow,
        paint(use_color, dim, &format!("· {meta} ·")),
        paint(use_color, style, run.conclusion.label()),
    );

    for job in &run.jobs {
        let _ = writeln!(
            out,
            "      {} {}",
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
                "          {}",
                paint(
                    use_color,
                    dim,
                    &format!("step {}: {}", step.number, step.name)
                ),
            );
        }
        if !job.url.is_empty() {
            let _ =
                writeln!(out, "          {}", paint(use_color, dim, &job.url));
        }
    }
    // A final jump-to link for the run as a whole.
    if !run.url.is_empty() {
        let _ = writeln!(out, "      {}", paint(use_color, dim, &run.url));
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

/// Render the report as a single pretty-printed JSON document.
pub fn json(report: &Report) -> String {
    let mut s = serde_json::to_string_pretty(report)
        .expect("Report always serializes to JSON");
    s.push('\n');
    s
}

/// Render the report as JSONL: one line per run, plus one per repo error,
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
        for run in &repo.runs {
            let mut value =
                serde_json::to_value(run).expect("RunReport always serializes");
            // Tag each run record with its origin so lines stand alone.
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("type".to_string(), serde_json::json!("run"));
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
    use crate::model::Report;

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
    fn jsonl_has_one_record_per_run_plus_per_error() {
        let report = Report::stub(1);
        let expected_runs: usize =
            report.repos.iter().map(|r| r.runs.len()).sum();
        let expected_errors =
            report.repos.iter().filter(|r| r.error.is_some()).count();
        let out = jsonl(&report);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), expected_runs + expected_errors);

        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)
                .expect("each jsonl line is valid JSON");
            let kind = value["type"].as_str().expect("each record is tagged");
            assert!(kind == "run" || kind == "repo_error");
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
        assert!(out.contains("no completed runs"), "neutral repo is shown");
        assert!(out.contains("403 Forbidden"), "repo error is shown");
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
    fn only_failures_hides_passing_runs_in_human() {
        let report = Report::stub(1);
        let out = human(&report, false, true);
        // The passing-only repo's whole block (header included) is gone; the
        // failing repo and its run stay, as does the errored repo.
        assert!(
            !out.contains("bbugyi200/actstat"),
            "a repo with only passing runs is hidden"
        );
        assert!(out.contains("bbugyi200/dotfiles"), "failing repo stays");
        assert!(out.contains("#128"), "the failing run stays");
        assert!(out.contains("bobs-org/locked"), "errors are still shown");
        assert!(
            !out.contains("no completed runs"),
            "neutral repos are hidden under --only-failures"
        );
    }

    #[test]
    fn human_run_line_shows_relative_time() {
        // Stub `generated_at` is 12:00:00Z; the passing run finished at
        // 11:52:30Z (7m30s earlier) and the failing one at 11:44:10Z (15m50s).
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(out.contains("7m ago"), "passing run shows relative time");
        assert!(out.contains("15m ago"), "failing run shows relative time");
    }

    #[test]
    fn human_failure_line_carries_conclusion_label() {
        let report = Report::stub(1);
        let out = human(&report, false, false);
        assert!(
            out.contains("· failure"),
            "non-success runs label the conclusion explicitly"
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
  ✔ CI · master · #42 · 7m ago

bbugyi200/dotfiles
  ✘ CI · feature/shell · #128 · 15m ago · failure
      ✘ test (3.14)
          step 5: Run tests
          https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003
      https://github.com/bbugyi200/dotfiles/actions/runs/2002

• sase-org/example  no completed runs

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
