---
create_time: 2026-07-07 11:59:48
status: wip
prompt: sdd/prompts/202607/single_running_actions_run.md
---
# Plan: Show only the most recently started running GitHub Actions run

## Goal

Narrow the active-runs feature so `actstat` shows **at most one running GitHub Actions run per repository**:

- Only GitHub workflow runs whose status is `in_progress` should be considered running.
- Queued, waiting, pending, requested, and completed runs should not appear in the active section.
- If more than one run is currently running for a repository, show the one that most recently started.
- If no run is currently running, the repository has no active entry; settled commits continue to behave exactly as they
  do today.

This keeps the output focused on "what is executing right now" instead of turning the active section into a live queue
view.

## Current State

The current implementation fetches one unfiltered `/actions/runs?per_page=100` page per repository, filters out only
`status == "completed"`, groups every remaining run by commit, and renders all non-completed runs. That means queued,
waiting, pending, requested, and in-progress runs can all be shown, and multiple active commits/runs can appear for a
single repo.

The existing model and render shape is:

```text
Report -> RepoReport -> ActiveCommitReport -> ActiveRunReport
```

Machine output uses `repositories[].active` in JSON and `type: "active_commit"` in JSONL. That shape is useful and
should remain stable; the meaning becomes narrower: the array will contain zero or one active commit, and that commit
will contain exactly one running run.

## Desired Semantics

### What Counts As Running

Use GitHub's workflow-run `status == "in_progress"` as the only running state. Do not treat these statuses as
active/running:

- `queued`
- `waiting`
- `pending`
- `requested`
- `completed`
- any conclusion-like status such as `success`, `failure`, `cancelled`, etc.

The GitHub REST docs for "List workflow runs for a repository" support a `status` query parameter and include
`in_progress` as an accepted value:

https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository

### Which Running Run Wins

For each repository:

1. Fetch a bounded all-branch window of running workflow runs.
2. Select only `status == "in_progress"` defensively, even though the request also asks GitHub for that status.
3. Pick the run with the most recent parsed `run_started_at`.
4. If `run_started_at` is absent or unparsable, fall back to parsed `created_at`.
5. If timestamps are tied or unavailable, use a deterministic fallback such as highest run id, then preserve API order
   as the final tie-breaker.

This makes "most recently started" explicit rather than relying on GitHub's listing order, which is not the same
semantic guarantee.

### Branch Scope

Keep the active lookup across all branches, with no `branch=` filter. A running PR workflow is just as relevant to "what
is running right now" as a default-branch workflow, and the existing output already shows the branch for context.

### Output Shape

Keep the existing `active` model shape for compatibility:

- JSON: `repositories[].active` remains always present.
- JSONL: `active_commit` remains the active record type.
- Human: active entries still render above settled commits.

The new invariant is:

- `repo.active.len()` is `0` or `1`.
- If present, `repo.active[0].runs.len()` is `1`.
- The only status emitted by collection is `in_progress`.

## Implementation Scope

### A. Model (`src/model.rs`)

- Update comments to describe running-only active data instead of queued/running/waiting in-flight data.
- Update `Report::stub` so its active example has exactly one `in_progress` run and no queued run.
- Simplify active rollup behavior:
  - The commit-level active label can be a literal `running`, since active commits are now running-only.
  - Either reduce `RunStatus` to the single emitted variant `InProgress`, or keep the enum but update tests/comments so
    non-running variants are not part of the active collection contract. Prefer the smaller truthful model if the
    compile fallout stays local.
- Keep `Report::has_failures()` unchanged; running runs are informational and must not affect exit codes.

### B. GitHub Collection (`src/github.rs`)

- Change active fetch from:

```text
/repos/{owner}/{repo}/actions/runs?per_page=100
```

to:

```text
/repos/{owner}/{repo}/actions/runs?status=in_progress&per_page=100
```

- Continue omitting `branch`.
- Replace `select_active_commits` with a running-only selector, for example:

```rust
select_most_recent_running_commit(runs: Vec<ApiRun>) -> Option<SelectedCommit>
```

- The selector should:
  - filter strictly to `status == Some("in_progress")`;
  - compare by parsed `run_started_at`, then parsed `created_at`, then deterministic fallback;
  - wrap the winning run in one `SelectedCommit` so existing `ActiveCommitReport` construction can be reused;
  - never preserve duplicate workflow runs, because the product requirement is now singular.
- `collect_active_commits` can keep returning `Vec<ActiveCommitReport>` for compatibility, mapping `None` to `vec![]`
  and `Some` to a one-item vector.
- Preserve the existing concurrent per-repo pipeline: when active lookup is enabled, run active and settled collection
  with `tokio::join!`; when `--no-active` is set, issue no active request.
- Preserve failure isolation: a GitHub error from the running-run lookup still becomes that repo's error row, matching
  the current active-fetch behavior.

### C. Rendering (`src/render.rs`)

- Keep active commits above settled commits.
- Update human output expectations:
  - no queued child run;
  - no "2 workflows" active metadata in the deterministic example;
  - active label is always `running`;
  - active run line always has `in_progress`.
- Simplify status styling if `RunStatus` becomes single-variant.
- Keep `--only-failures` behavior unchanged: hide active/running entries in human output because they are not failures;
  JSON and JSONL still include the data.
- Keep JSON and JSONL serialization shape unchanged; only the example contents and test expectations change.

### D. CLI (`src/cli.rs`)

- Keep `--no-active`; it still means "skip the extra active lookup".
- Update help/diagnostic wording from broad "queued/running in-flight runs" to "running workflow run" or "running-run
  lookup".
- Keep empty-report suppression logic as-is: a repo with only a running active entry should remain visible; a repo with
  only queued runs should not be retained by active data because queued runs are no longer collected.
- Keep `--fail-on-failure` and exit-code behavior unchanged.

### E. Tests

Update existing tests instead of layering new behavior on top of the old queued-inclusive contract.

Model tests:

- Stub contains one active run with `status == InProgress`.
- Active rollup label is `running`.
- If `RunStatus` is reduced, remove round-trip coverage for queued/waiting/pending/requested; if variants remain, make
  clear they are not selected by collection.

Pure collection tests:

- The selector ignores queued, waiting, pending, requested, completed, and unknown statuses.
- The selector returns no active commit when no run is `in_progress`.
- The selector chooses the greatest valid `run_started_at`, even when API order differs.
- The selector falls back to `created_at` when `run_started_at` is missing.
- The selector uses a deterministic tie-breaker when timestamps are equal or malformed.
- The selected active commit contains exactly one run.

Mocked HTTP tests:

- Active fetch sends `status=in_progress` and `per_page=100`, with no `branch`.
- A repo with one running run plus settled commits reports both halves.
- A repo with only queued runs returns no active commits and, if it has no settled commits, is suppressible later.
- `--no-active` still issues no running-run request.
- Active-fetch failure still becomes a repo error row.

Render/CLI tests:

- Update the no-color human snapshot.
- JSON always carries `active`, now empty or single-entry.
- JSONL emits at most one `active_commit` per repo before settled commits.
- `--only-failures` hides running entries in human output only.
- Active-only running repos are retained by suppression; queued-only repos are not retained by active data.

### F. README

Update user-facing docs to match the narrower behavior:

- Intro/highlights: "currently running run" instead of "queued/running/waiting in-flight runs".
- Options table: `--no-active` skips the running-run lookup.
- Human example: remove the queued workflow line and plural active metadata.
- JSON example: `active` contains a single `in_progress` run.
- JSONL example: one `active_commit` record with one run.
- Exit codes: running runs never affect exit codes.
- Troubleshooting:
  - a queued-only workflow will not appear in active output;
  - once GitHub marks a run `in_progress`, the most recently started running run appears unless `--no-active` is used;
  - active detection is bounded to the newest 100 `in_progress` records returned by GitHub.

## Verification

1. Run `cargo fmt -- --check`.
2. Run `cargo clippy --all-targets -- -D warnings`.
3. Run `cargo test`.
4. Run `just check` as the final project gate.
5. Optional live smoke:
   - trigger or catch a real GitHub Actions run;
   - confirm `actstat` shows only one `in_progress` run for that repo;
   - confirm queued runs do not appear;
   - confirm `actstat --no-active` hides the running entry;
   - confirm the run later migrates to settled output after completion.

## Design Decisions

- **Use `status=in_progress` server-side.** The previous unfiltered request was appropriate when the feature covered
  multiple non-completed statuses. With a single accepted status, the filtered request is more precise and reduces the
  chance that the relevant running run is pushed out of the first page by queued or completed runs.
- **Keep all-branch scope.** "What is running right now?" should include PR and feature-branch workflows; the branch
  field supplies context.
- **Keep the existing machine-output shape.** Changing `active` to a flat `running_run` field would be cleaner in
  isolation, but it would break the just-added JSON/JSONL contract unnecessarily. A zero-or-one active array is a
  smaller, compatible semantic change.
- **Sort by started time, not list order.** The user asked for "most recently started"; `run_started_at` is the direct
  field for that. `created_at` is only a robustness fallback.
- **Running runs remain non-gating.** A running workflow has not failed or passed yet, so it should never influence
  `--fail-on-failure`.

## Out of Scope

- Showing queued/waiting/pending/requested runs.
- Showing multiple running runs per repository.
- Adding a separate `--show-queued` or `--active-limit` flag.
- Paginating beyond the first 100 `in_progress` runs.
- Active job/step detail for currently executing jobs.
