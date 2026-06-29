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
pub fn human(report: &Report, use_color: bool, only_failures: bool) -> String {
    let mut out = String::new();
    let bold = Style::new().bold();
    let dim = Style::new().dimmed();

    for repo in &report.repos {
        // Per-repository error: a clear, self-contained row.
        if let Some(err) = &repo.error {
            let _ = writeln!(
                out,
                "{} {}  {}",
                paint(use_color, Style::new().red(), "✘"),
                paint(use_color, bold, &repo.repo),
                paint(use_color, Style::new().red(), err),
            );
            continue;
        }

        let visible_runs: Vec<&RunReport> = repo
            .runs
            .iter()
            .filter(|r| !only_failures || !r.is_success())
            .collect();

        // Neutral: no completed runs (never silently disappears). Skipped when
        // --only-failures hides everything for this repo.
        if repo.runs.is_empty() {
            if !only_failures {
                let _ = writeln!(
                    out,
                    "{} {}  {}",
                    paint(use_color, dim, "•"),
                    paint(use_color, bold, &repo.repo),
                    paint(use_color, dim, "no completed runs"),
                );
            }
            continue;
        }

        if visible_runs.is_empty() {
            continue;
        }

        let _ = writeln!(out, "{}", paint(use_color, bold, &repo.repo));
        for run in visible_runs {
            render_run(&mut out, run, use_color);
        }
    }

    if out.is_empty() {
        let _ = writeln!(out, "{}", paint(use_color, dim, "no runs to report"));
    }
    out
}

/// Render a single run: a compact line for passes, an expanded tree for fails.
fn render_run(out: &mut String, run: &RunReport, use_color: bool) {
    let style = conclusion_style(run.conclusion);
    let dim = Style::new().dimmed();

    let _ = writeln!(
        out,
        "  {} {} {}  {} {}",
        paint(use_color, style, run.conclusion.icon()),
        run.workflow,
        paint(use_color, dim, &format!("({})", run.branch)),
        paint(use_color, dim, &run.title),
        paint(use_color, dim, &format!("#{}", run.run_number)),
    );

    if run.is_success() {
        return;
    }

    // Expanded failure detail: failed jobs → failed steps → URLs.
    for job in &run.jobs {
        let _ = writeln!(
            out,
            "      {} {} {}",
            paint(use_color, conclusion_style(job.conclusion), "↳"),
            job.name,
            paint(use_color, dim, &job.url),
        );
        for step in &job.steps {
            let _ = writeln!(
                out,
                "          {} {}",
                paint(use_color, conclusion_style(step.conclusion), "·"),
                step.name,
            );
        }
    }
    let _ = writeln!(out, "      {}", paint(use_color, dim, &run.url));
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
        // The passing run's title must be gone; the failing one stays.
        assert!(!out.contains("Add list subcommand"));
        assert!(out.contains("Refactor shell init"));
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
