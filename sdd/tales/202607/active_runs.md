---
create_time: 2026-07-07 11:41:33
status: done
prompt: sdd/prompts/202607/active_runs.md
---
# Plan: Show currently-running (in-flight) GitHub Actions runs per project

## Goal

`actstat` today reports only **settled** commits — commits whose workflow runs have all completed. Anything currently
queued or running is invisible; worse, the _newest_ commit on a repo's default branch disappears from the output while
its CI is in flight (a documented Troubleshooting wart: "The newest GitHub commit is absent"). Add a first-class
**active runs** view: for every repository shown, also display any not-yet-completed workflow runs (queued, in progress,
waiting, …) — intuitive, reliable, and beautiful, in all three output formats.

## Design overview (what the user sees)

Active runs are grouped **by commit** — the same commit-centric tree shape the whole tool renders
(`repo → commit → runs`) — and appear **above** the settled commits inside each repo block, since they are strictly
newer. An active commit line mirrors a red commit line (icon · SHA · title · metadata · trailing state label) and always
expands into its active runs, exactly the way a red commit expands into its problem runs. Cyan is the "activity" color
(green = pass, red = fail, yellow = warn, cyan = in flight). Target no-color human output:

```text
bbugyi200/actstat
  ↻ f00ba12 Add progress spinner · master · 2 workflows · 1m20s · running
      ↻ CI · #44 · 1m20s · in_progress
          https://github.com/bbugyi200/actstat/actions/runs/1044
      ⧖ Deploy Docs · #13 · queued
          https://github.com/bbugyi200/actstat/actions/runs/1045
  ✔ a1b2c3d Add list subcommand · master · 2m30s · 7m ago
```

Reading it:

- **Commit line** — `↻` (cyan) + short SHA + title, then the familiar `·`-separated metadata: branch, workflow count
  when > 1, **elapsed time so far** (earliest active-run start → `generated_at`, rendered with the existing
  `humanize_duration`), and a trailing aggregate state label: `running` if any run is in progress, else `queued`.
  Mirrors the red-commit line, which ends in `· failure`.
- **Run lines** — one per active run (always expanded, like problem runs on a red commit): status glyph, workflow name,
  `#run_number`, elapsed time (omitted when the run hasn't started), and the GitHub status label (`in_progress`,
  `queued`, `waiting`, …). Each run carries its dim URL child line — jumping to a _live_ run log is the primary action
  here, matching how failed runs link to their logs.
- **Glyphs** — `↻` for `in_progress`; `⧖` for the not-yet-running states (`queued`, `pending`, `requested`); `⧗` for
  `waiting` (deployment-protection gates). All single-width, consistent with the existing `✔ ✘ ⏱ ⊘ ⚑ •` set.

Because unsettled commits are exactly the ones the settled selector skips, a commit never appears in both sections — the
active section naturally "fills the gap" that used to make the newest commit vanish.

## Scope

### A. Model (`src/model.rs`) — new active types, no `Option<Conclusion>` contagion

Active runs have genuinely different facts than completed runs (a _status_ instead of a _conclusion_; an _elapsed_
notion instead of a _duration_), so they get their own small types rather than threading `Option`s through
`RunReport`/`CommitReport`:

- **`RunStatus`** enum: `Queued`, `InProgress`, `Waiting`, `Pending`, `Requested`, with `label()` (snake_case, matching
  GitHub), `icon()` (glyphs above), and `from_github(Option<&str>) -> RunStatus`. Unknown/absent statuses map to
  `InProgress`: anything non-completed is by definition in flight, and "running" is never a dangerous misread (unlike
  conclusions, where unknown must map loudly to `Failure`). Serialize snake_case.
- **`ActiveRunReport`**: `workflow`, `title`, `run_number`, `event`, `branch`, `sha` (7-char), `status: RunStatus`,
  `url`, `created_at`, `started_at: Option<String>` (from `run_started_at`). No stored elapsed — elapsed is "as of
  `generated_at`", so the human renderer derives it (see C), keeping the model timestamp-honest like `duration_seconds`
  is for completed runs.
- **`ActiveCommitReport`**: `sha`, `title`, `branch`, `event`, `url` (commit URL), `started_at` (earliest across its
  runs' `run_started_at`/`created_at`), `runs: Vec<ActiveRunReport>`, plus a rollup helper (`running` if any run is
  `InProgress`, else `queued`) for the trailing label.
- **`RepoReport`** gains `active: Vec<ActiveCommitReport>`, ordered before `commits` in both the struct and JSON. Always
  serialized (even `[]`) so `jq` consumers get a predictable shape.
- **`Report::has_failures()` is untouched** — active runs are never failures and never gate exit codes.
- **`Report::stub`** grows an active commit (one `in_progress` + one `queued` run) on the `bbugyi200/actstat` entry so
  every rendering path is exercised deterministically; the snapshot test updates accordingly.

### B. Collection (`src/github.rs`) — one extra bounded call per repo, fetched concurrently

- **New fetch**: `GET /repos/{owner}/{repo}/actions/runs?per_page=100` — _no_ `branch` and _no_ `status` filter. The
  listing is newest-first by creation; keep every run with `status != "completed"` client-side. One call covers **all
  branches** (PR-triggered runs are the bulk of real in-flight activity) and **every** non-completed status without
  enumerating N `status=`-filtered queries. The 100-newest-runs window bound matches the existing settled-commit design
  (also a single non-paginated window); the theoretical miss — an active run older than a busy repo's last 100 runs — is
  accepted and documented.
- **Grouping**: a pure `select_active_commits(runs) -> Vec<SelectedCommit>` filters non-completed runs and groups by
  full head SHA in first-appearance (newest-first) order — factor the group-by-SHA logic shared with
  `select_settled_commits` into a small helper. No per-workflow dedup: two simultaneously-active runs of one workflow
  (e.g. queued behind a concurrency group) are genuinely distinct and both shown.
- **Normalization**: `normalize_active_run(ApiRun) -> ActiveRunReport` (7-char SHA, `RunStatus::from_github`, absent
  fields degrade to defaults exactly like `normalize_run`), and a `build_active_commit_report` that derives
  title/event/branch from the first run and the earliest start. **No jobs expansion** for active runs — no extra API
  calls, and in-flight job detail is volatile (future work).
- **Per-repo pipeline**: the per-repo op now returns both halves (e.g. a small `RepoStatus { active, commits }`), so
  `fetch_repo_reports`'s generic op and `collect_repo_reports` update accordingly. The active fetch runs **concurrently
  with** the existing settled chain via `tokio::join!`. When active collection is disabled (see D) the extra request is
  simply never issued.
- **Failure isolation unchanged**: any fetch error for a repo (active or settled) becomes that repo's error row, never
  aborting the run.

### C. Rendering (`src/render.rs`)

- **Human**: render `repo.active` commits before settled commits (format above). Elapsed values are derived from
  `started_at`/`created_at` against `report.generated_at` — deterministic for a given report, reusing the existing
  timestamp-parsing/`humanize_duration` helpers. `--only-failures` **hides** active commits (they are not failures); a
  repo with only active commits still renders normally otherwise. Byte-clean without color, as ever.
- **JSON**: `repositories[].active` appears (always, even empty) with the new nested shape — purely additive for
  existing consumers.
- **JSONL**: a new record `type: "active_commit"` (one line per active commit, tagged with `repo`, carrying its `runs`),
  emitted per repo after any `repo_error` line and before its `commit` lines — mirroring the human ordering.
- Add a `status_style` (cyan for `InProgress`, dimmed-cyan or yellow for the waiting states — pick in implementation,
  favoring cyan family) beside `conclusion_style`.

### D. CLI (`src/cli.rs`)

- **`--no-active` flag** on `ListArgs`: "Skip fetching and showing in-flight (queued/in-progress) workflow runs."
  Default **on** (the user asked for this to just start appearing). The flag skips the extra API call entirely — useful
  for cron gates that only care about settled status and want minimal API usage.
- **Exit codes unchanged**: active runs never trip `--fail-on-failure` and never count toward "all repos errored". A
  repo showing only active commits exits `0` even with `--fail-on-failure`.
- **Empty-row suppression**: `suppress_empty_success_reports` now retains a repo when it has commits **or active
  commits** or an error.
- The verbose "inspecting N repositories" diagnostic can note whether active runs are included.

### E. Tests (mirroring existing style; wiremock, no network)

- **Model**: `RunStatus` label/`from_github` round-trip; unknown status falls back to `InProgress`; active-commit rollup
  label (`running` vs `queued`).
- **Collection (pure)**: `select_active_commits` drops completed runs, groups by SHA newest-first, keeps duplicate
  workflows; `normalize_active_run` maps/shortens fields and degrades absent fields cleanly.
- **Collection (mocked HTTP)**: the active fetch hits `/actions/runs` with `per_page=100` and _no_ `branch`/`status`
  params (`query_param_is_missing`); a repo with one running commit and one settled commit reports both; an active-only
  repo (nothing settled) survives suppression; `--no-active` issues no unfiltered listing request (`.expect(0)` mock);
  an active-fetch HTTP failure becomes the repo's error row.
- **Render**: updated full no-color snapshot (the authoritative layout spec); `--only-failures` hides active commits in
  human output while JSON/JSONL keep them; JSONL emits tagged `active_commit` records in the documented order; JSON
  always carries `active`; no-color output stays byte-clean.
- **CLI**: `--no-active` parses (bare and under `list`); exit-code table unaffected by active-only repos.

### F. README

- **Highlights**: new bullet — in-flight runs are shown live, queued/running, above the settled history.
- **Output → Human**: extend the example block and its "reading the example" list with the active commit lines.
- **`list` options table**: add `--no-active`.
- **JSON / JSONL sections**: document the `active` array and the `active_commit` record type with examples.
- **Troubleshooting**: rewrite "The newest GitHub commit is absent" — it now appears as an active commit while its runs
  are in flight (unless `--no-active`); note the 100-newest-runs window bound and that active runs never affect exit
  codes.

## Files changed

- `src/model.rs` — `RunStatus`, `ActiveRunReport`, `ActiveCommitReport`; `RepoReport.active`; extended stub; tests.
- `src/github.rs` — unfiltered runs fetch, `select_active_commits`, `normalize_active_run`, concurrent per-repo
  collection returning active + settled; tests.
- `src/render.rs` — active sections in human/JSON/JSONL, status styling, elapsed derivation; updated snapshot + tests.
- `src/cli.rs` — `--no-active`, suppression rule update, wiring; tests.
- `README.md` — highlights, output examples, options table, JSON/JSONL schemas, troubleshooting.

## Verification

1. `just check` — fmt-check, clippy `-D warnings`, and the full test suite (including the new snapshot) pass.
2. Live smoke test: kick off a real run (`gh workflow run` or a push) on a configured repo, then `actstat` — the running
   commit appears with `↻`, its run URL opens the live log; `actstat --no-active` hides it; once CI settles, the commit
   migrates to the settled section on the next invocation.
3. `actstat -f json | jq '.repositories[].active'` — always an array;
   `actstat -f jsonl | jq 'select(.type=="active_commit")'` yields the running records.
4. `actstat --fail-on-failure` against a repo that is green but mid-run exits `0`.

## Design decisions (and alternatives considered)

- **Commit-grouped active section, not a flat run list.** The whole tool renders `repo → commit → runs`; grouping active
  runs under their commit keeps one visual grammar, avoids repeating commit titles per run, and mirrors how red commits
  expand. Flat lists read fine for one run but degrade with multi-workflow repos.
- **One unfiltered window call vs. per-status filtered calls.** `status=` accepts one value per request, so precise
  coverage of queued/in_progress/waiting/pending/requested would cost up to 5 calls per repo. One unfiltered
  `per_page=100` call catches every active status within the window at a single call — keeping the README's "small and
  conservative number of GitHub API calls" promise — and window-bounding is already this tool's stated design for
  settled selection. The rare stale-`waiting` run beyond the window is a documented, accepted miss.
- **All branches, not default-branch-only.** Default-branch-only would cost _zero_ extra calls (the settled fetch
  already sees those runs) but misses PR-triggered runs — the majority of real in-flight CI. Showing all branches is
  what "what's running right now?" intuitively means; the branch name on each active commit line provides the context.
- **Distinct `Active*` types instead of `Option<Conclusion>` on existing types.** Keeps `conclusion` always-terminal,
  keeps JSON schemas honest (`status` vs `conclusion`), and spares every renderer from impossible-state handling.
- **Active runs never gate exit codes.** In-flight is not failure; cron/CI semantics (`--fail-on-failure`, exit `2`)
  stay exactly as documented.
- **Unknown status → `InProgress`** (vs. unknown conclusion → `Failure`): both fallbacks choose the safe loud-enough
  reading for their context — an unknown _conclusion_ must never pass silently, while an unknown _non-completed status_
  is by definition in flight.

## Out of scope

- **Job/step detail for active runs** (which job/step is executing) — one more API call per active run and highly
  volatile; a natural follow-up.
- **A `--watch` / auto-refresh mode** — this stays a one-shot report; re-run (or cron) to refresh.
- **Pagination beyond the 100-run window** for active detection (consistent with settled selection's window).
- Per-repo config to disable active fetching; the global `--no-active` flag suffices.
