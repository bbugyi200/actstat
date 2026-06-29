---
create_time: 2026-06-29 09:24:15
status: wip
prompt: sdd/prompts/202606/commit_centric_status.md
---
# Plan: Commit-centric `actstat list` (settled-commit selection + empty-repo suppression)

## Problem / motivation

`actstat list` today is **run-centric**: per repository it fetches the _N most recent completed workflow runs_
(`GET …/actions/runs?status=completed&per_page=N`) and renders one line per run. Three problems follow from that model:

1. **Empty repos are noisy.** A repository with no completed runs still prints a neutral
   `• owner/name  no completed runs` row (and an empty `runs: []` in the machine formats). The user wants output only
   for repos that actually have runs.

2. **It is not commit-aware.** A single commit usually triggers several workflow runs (e.g. `sase-org/sase` commit
   `e84474f` ran `CI` = failure, `Deploy Docs` = success, `Publish` = success). The run-centric view can show a green
   `Deploy Docs` line and bury the fact that `CI` failed on that same commit. The user wants the unit to be the
   **commit**: show the most recent commit whose CI has **completely finished**, with an aggregate pass/fail for the
   whole commit.

3. **Partial / in-progress commits mislead.** The newest commit on a branch often still has runs `queued`/`in_progress`.
   Reporting it produces a premature or half-true status. The user wants the most recent commit that is _completely
   finished_ (every workflow run for that commit has settled).

The user also clarified: a workflow run that aborted quickly because some of its jobs failed (so few/no jobs ran to
completion) is still a **failed** workflow and its failed jobs must be shown; and when multiple jobs fail, **all**
failed jobs must be shown — never just one.

### Approved design decisions (from the review Q&A)

- **Q1 — Branch scope: default branch only.** Consider only runs on the repo's default branch (e.g. `master`/`main`).
  This naturally drops `release-please` / PR-automation noise, because a `pull_request` run's head branch is the PR
  source branch, not the default branch.
- **Q2 — Commit aggregation: fail if any run failed.** A commit is RED if any of its workflow runs is non-passing; show
  every failed run's failed jobs. A commit is GREEN only if all of its runs passed.
- **Q3 — `-n`/`--limit` now counts commits.** It selects the _N most recent settled commits_ per repository (each
  rendered as one commit summary).
- **Q4 — Empty-repo suppression everywhere; keep errors.** Drop repos with no qualifying commits from human, JSON, and
  JSONL output. Still show repos that errored (403/404/rate-limited/etc.).

### Key semantics: what "completely finished" means (validated against live data)

A commit is **settled / completely finished** when **every** workflow run it triggered on the default branch has
`status == "completed"` (none `queued`, `in_progress`, `waiting`, `requested`, or `pending`). We select the most recent
settled commits and skip any newer commit that still has a non-completed run.

This was confirmed against `sase-org/sase` (default branch `master`), newest → oldest, where each line is one workflow
run (`status/conclusion · workflow`):

```
9b93600  queued/-       CI            ─┐
9b93600  in_progress/-  Deploy Docs    │ newest commit — NOT settled (CI queued,
9b93600  completed/✓    Publish       ─┘ Deploy Docs still running) → skip
a3e494d  queued/-       CI             … NOT settled (CI still queued) → skip
b7e9829  in_progress/-  CI             … NOT settled (CI still running) → skip
d44ab2e  completed/✗    CI            ─┐ first SETTLED commit → SHOW: RED
d44ab2e  completed/✓    Deploy Docs   ─┘ (CI failed; aggregate = failure)
e84474f  completed/✗    CI            … settled → RED
…
f33532d  completed/✓    CI / Deploy Docs / Publish   … settled → GREEN one-liner
```

So for `sase-org/sase`, `-n 1` correctly reports `d44ab2e` as a CI **failure** — the most recent fully-settled master
commit — instead of the half-running newest commit or the release-please PR noise. This is exactly the desired behavior.

## Design

### New normalized model: a commit layer

Introduce a `CommitReport` between `RepoReport` and `RunReport`. The model tree becomes:

```
Report → RepoReport → CommitReport → RunReport → JobReport → StepReport
```

- `RepoReport.runs: Vec<RunReport>` is replaced by `RepoReport.commits: Vec<CommitReport>` (JSON key `commits`).
- `CommitReport` carries the commit-level rollup:
  - `sha` (short 7-char), `title` (commit message / run `display_title`), `branch` (the default branch), `event`
    (representative run's event), `conclusion` (aggregate; see below), `url`
    (`https://github.com/{owner}/{name}/commit/{full_sha}`), `finished_at` (the latest `updated_at` across the commit's
    runs — what the relative "x ago" is measured from), `duration_seconds: Option<u64>` (aggregate wall-clock = latest
    finish − earliest start across the commit's runs; honestly omitted when timestamps are missing — reuses the existing
    duration helper/skip-if-none behavior),
  - `runs: Vec<RunReport>` — all of the commit's completed workflow runs (each normalized; the failed/problem ones
    enriched with their failed jobs and steps). This keeps the machine formats complete while letting the human renderer
    collapse a green commit to one line.
- Aggregate conclusion: a commit's `conclusion` is `Success` when **none** of its runs is an actionable problem (reusing
  the existing `is_problem` taxonomy — `success`/`skipped`/`neutral` are not problems); otherwise `Failure`. Each failed
  run still carries its own precise conclusion label (`failure`, `cancelled`, `timed_out`, …) in the detail, so
  specificity is not lost.
  - Design note to call out for review: this treats `skipped`/`neutral` runs as non-failing (a path-filtered or no-op
    workflow does not turn a commit red), consistent with how jobs/steps are already filtered. If the reviewer wants
    _strict_ "green only if every run is literally `success`", that is a one-line change to the aggregate predicate.
- `RepoReport::has_failures` and `Report::has_failures` are updated to fold over `commits` (a repo has failures if any
  shown commit is non-success), preserving the `--fail-on-failure` exit-code contract.
- `Report::stub` is rewritten to the commit-centric shape, still covering every render path: a green single-workflow
  commit, a red commit with a failed run + failed job/step (and at least one _passing_ sibling run on the same commit to
  exercise aggregation), and a repo error row. The previous "neutral empty repo" stub entry is removed because empty
  repos are now suppressed.

### GitHub data access (`src/github.rs`)

Per repository, the new pipeline (`collect_commits`, replacing `collect_runs`):

1. **Resolve the default branch.** `GET /repos/{owner}/{repo}` → `default_branch` (new minimal
   `ApiRepoDetail { default_branch }`). A failure here is isolated into the repo's `error` row exactly as today.
2. **List default-branch runs without a status filter.**
   `GET /repos/{owner}/{repo}/actions/runs?branch={default_branch}&per_page=100`. Dropping `status=completed` is
   essential: it is the only way to _see_ the `queued`/`in_progress` runs that mark a commit as not-yet-settled.
   `branch=` enforces Q1 (default branch only). One page of 100 runs comfortably covers the N most-recent commits in
   practice; the selection window size is documented and the count of runs fetched is logged under `-v`.
3. **Group + select settled commits** via a pure, unit-tested function `select_settled_commits(runs, limit)`:
   - Group runs by full `head_sha`, preserving first-appearance order (the API returns newest `created_at` first, so
     this is "most recent commit first").
   - Within a commit, collapse to the **latest run per workflow** (highest run id) so a re-run supersedes its earlier
     attempt.
   - A commit is **settled** when all of its (deduped) runs have `status == "completed"`. Take the first `limit` settled
     commits in order; skip non-settled ones.
4. **Aggregate + enrich.** For each selected commit, build a `CommitReport`: compute the aggregate conclusion; for a RED
   commit, enrich every problem run with its failed jobs/steps via the existing `list_run_jobs` (so _all_ failed jobs
   across _all_ failed runs of the commit are shown); a GREEN commit needs no job requests.
5. Requires adding `status: Option<String>` to `ApiRun` (to detect "completed") and keeping the full `head_sha` for
   grouping + the commit URL.

`collect_repo_reports` and the `fetch_repo_reports` fan-out/error-isolation machinery are reused unchanged except that
the per-repo op now returns `Vec<CommitReport>`.

### Empty-repo suppression (Issue 1 / Q4)

After collection (and after folding in org-expansion error rows), drop any repo with **no commits and no error**:
`reports.retain(|r| !r.commits.is_empty() || r.error.is_some())`. Doing it once, in the assembly stage, makes the
suppression consistent across all three formats automatically and keeps the renderers simple. Error rows (and
org-expansion error rows) are always kept.

### Rendering (`src/render.rs`)

- **Human.** Group by repo (unchanged framing). For each repo, render its commits:
  - GREEN commit → one compact line: icon · short-sha · title · branch · (optional workflow count) · duration · relative
    finished time.
  - RED commit → that summary line + aggregate label, then a nested tree: failed workflow run (workflow name · run
    number · duration · its conclusion label) → failed jobs → failed steps → job/run URLs.
  - `--only-failures` now hides **green commits** (and a repo whose every shown commit is green disappears entirely),
    keeping the existing intent. Error rows are still always shown.
  - The "no completed runs" neutral row is removed (empty repos are suppressed).
- **JSON.** Same single document; each repo now carries `commits` (each with its `runs` → `jobs` → `steps`) instead of
  `runs`.
- **JSONL.** One record per **commit** (`type: "commit"`, carrying `repo`, the aggregate fields, and nested `runs`) plus
  one `repo_error` record per errored repo. (Switching the per-line unit from run to commit matches the new model; the
  jq recipe in the README is updated accordingly.)
- The no-color human snapshot test and the `humanize_duration` / relative-time helpers are retained; the snapshot is
  updated to the new commit layout.

### CLI (`src/cli.rs`)

- `-n/--limit` help text and `value_name` change from "most-recent completed runs per repository" to "most-recent
  settled commits per repository" (still `≥ 1`, default `1`; the `parse_limit` validator is unchanged).
- Apply the empty-repo `retain` in `run_list` after `collect_repo_reports` + `org_error_reports`.
- Exit-code policy is unchanged in spirit and now folds over commits (`all_repos_errored` and `has_failures` operate on
  the suppressed report).
- The verbose diagnostic line wording is updated (commits, branch).

### README (`README.md`)

Update to the commit-centric model throughout: intro/highlights wording; the `-n` row and `--only-failures` note; the
Output section prose; the human, JSON, and JSONL examples (now commit-centric, with the `commits[]` shape and a
`type:"commit"` JSONL line); the jq examples (`.repositories[].commits[]` and the `type=="commit"` filter); the
"Contributing" model diagram (add `CommitReport`); and the Troubleshooting entries (remove "no completed runs" as a
neutral row; explain that repos with no settled commits are simply omitted, and that the most recent _settled_ commit on
the default branch is what gets reported). All shown example outputs will be regenerated to match real rendered output
byte-for-byte.

## Testing

- **`model.rs`**: rewrite `Report::stub`; add tests for the aggregate-conclusion predicate and `has_failures` over
  commits.
- **`github.rs`**: unit-test `select_settled_commits` for grouping order, per-workflow latest-run dedup, skipping
  non-settled (queued/in_progress) commits, and the `limit` cut; mock-HTTP tests for the full `collect_commits` path —
  default-branch resolution, a settled GREEN commit (no jobs request), a settled RED commit with multiple failed jobs
  across runs (all shown), a fast-aborted failed run still surfaced, and a repo whose only recent commit is unsettled →
  no commits (→ later suppressed). Update existing run-centric tests.
- **`render.rs`**: update all human/JSON/JSONL tests and the no-color snapshot to the commit shape; keep helper tests;
  add a test that a repo with no commits renders nothing while an error repo still renders.
- **`cli.rs`**: add a test that empty repos are suppressed while error repos are retained; update stub-dependent tests.

## Verification

1. Run the full project gate (format check, clippy with warnings-as-errors, and the test suite).
2. Run a live smoke command against the configured source file (token via the GitHub CLI, network permitting) and
   confirm: empty repos are absent; each shown repo reports its most recent _settled_ default-branch commit; a commit
   with a failed workflow is RED and lists all failed jobs; `-n 3` shows three commits. Spot-check against
   `sase-org/sase` that `-n 1` reports the most recent settled master commit (a CI failure), not the still-running
   newest commit.

## Out of scope

- Paginating beyond the first window of default-branch runs (the bounded window is sufficient for "recent commits";
  documented + logged under `-v`).
- Changing token discovery, org expansion, retry/backoff, or the config schema.
- Any new CLI flags (the change is to the meaning of existing `-n` and the selection logic, not the surface).
