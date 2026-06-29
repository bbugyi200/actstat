---
create_time: 2026-06-29 07:19:02
bead_id: actstat-1
tier: epic
status: wip
prompt: sdd/prompts/202606/actstat_init_4.md
---
# actstat Initialization Plan (Rust)

## Goal

Initialize `actstat`: a fast, beautiful Rust CLI that reports the status of the most recent completed GitHub Actions
workflow runs across a configured set of GitHub repositories. The first command is `actstat list`, and running `actstat`
with no subcommand behaves exactly like `actstat list`.

The tool is designed to be run interactively in a terminal **and** unattended from cronjobs, so it must be fast, quiet
about successes, loud and detailed about failures, conservative with the GitHub API, and emit clean machine-readable
output for scripting.

> NOTE: A prior auto-generated plan recommended Python. This plan supersedes it. `actstat` will be written in **Rust**,
> per the explicit performance requirement (cronjob usage, fan-out across many repositories).

## Product Direction

`actstat` answers one question fast: **"Which of my projects are healthy, and what broke most recently?"**

Default human output is compact for success and expands only for problems:

- A passing workflow run renders as a **single line**.
- A failing (or otherwise non-successful) run renders a headline plus **nested failed jobs and failed steps** with
  direct GitHub URLs, so the user can jump straight to the broken job log.
- Output is grouped by repository, repositories sorted alphabetically, runs sorted newest-completed-first.
- A repository with no completed runs renders as a clear neutral status, never silently disappears.
- A repository that errors (no access, Actions disabled, rate-limited) renders a clear per-repo error and never aborts
  the whole run.

Machine output is schema-oriented (not a terminal table serialized to text):

- `human` (default): adaptive, colorized terminal output that degrades gracefully without color/TTY.
- `json`: a single JSON document — metadata + repositories + runs + failed jobs + failed steps.
- `jsonl`: one record per line (one per workflow run, plus one per repository-level error) for easy `jq`/shell piping.

### CLI Contract

```
actstat [GLOBAL OPTS] [SUBCOMMAND]
actstat                 # == actstat list   (default subcommand)
actstat list [OPTS]
```

`list` options:

- `-n, --limit <N>` — number of most-recent **completed** runs to inspect per repository (default `1`). Applies
  per-repository, not globally.
- `-f, --format <human|json|jsonl>` — output format (default `human`).
- `-c, --config <PATH>` — explicit config path (overrides discovery).
- `--color <auto|always|never>` — color control (default `auto`; also honor the `NO_COLOR` env var).
- `--only-failures` — show only non-passing runs in human output (machine output still includes everything unless also
  filtered; documented behavior).
- `--repo <OWNER/NAME>` — repeatable; restrict this run to a subset of the configured repositories (handy for ad-hoc
  checks without editing config).
- `--concurrency <N>` — max concurrent repositories in flight (default a sensible small value, e.g. `8`).
- `--fail-on-failure` — exit non-zero if any inspected run is non-successful (for cronjobs/CI gating).
- `-v, --verbose` / `-q, --quiet` — diagnostic verbosity (stderr only; never pollutes machine output on stdout).

To support `actstat -n 3` as an alias for `actstat list -n 3`, use clap's `args_conflicts_with_subcommands = true` with
the `list` arguments flattened at the top level and an `Option<Subcommand>`; when no subcommand is given, dispatch to
`list` using the flattened args.

### Exit Codes

- `0` — ran to completion (workflow conclusions are reported, not gating) — the default.
- `1` — operational error (no usable config, config parse error, all repositories failed, fatal auth/network error).
- `2` — only when `--fail-on-failure` is set and at least one inspected run was non-successful.

Machine output always goes to **stdout**; diagnostics, warnings, and progress go to **stderr** so `actstat list -f json`
is always pipe-clean.

## Configuration Design

### Discovery order

1. `--config PATH`
2. `ACTSTAT_CONFIG` env var
3. `$XDG_CONFIG_HOME/actstat/config.yml`
4. `~/.config/actstat/config.yml`

If none exists, exit `1` with an actionable message that prints the expected path and a minimal example config.

### Schema

The config uses the user's own vocabulary ("projects") and a small, forward-compatible shape. An entry is either an
**org** (expands to all repos in that org) or a single **repo**.

```yaml
# ~/.config/actstat/config.yml
projects:
  - org: sase-org # all repositories in the sase-org organization
  - org: bobs-org # all repositories in the bobs-org organization
  - repo: bbugyi200/dotfiles # a single repository
  - repo: bbugyi200/actstat # this repository
```

Design notes / forward-compatibility (reserved, not all implemented in phase 1):

- `org: <name>` resolves to every repository the authenticated token can see in that org.
- `repo: <owner>/<name>` is exactly one repository.
- Org entries may later carry optional filters without a breaking change, e.g.:

  ```yaml
  - org: sase-org
    include_archived: false # default false
    include_forks: false # default false
    exclude: [sase-org/scratch]
  ```

- All sources resolve into a **de-duplicated, alphabetically stable** list of `owner/name`. A repo named both explicitly
  and via its org appears once.
- Optional future top-level `defaults:` block (e.g. default limit) is reserved but not required now.

### The real config lives in chezmoi

The actual machine config is added through chezmoi so it materializes at `~/.config/actstat/config.yml`. The chezmoi
source root is the `home/` subdirectory (per `.chezmoiroot`), so the file is created at:

```
home/dot_config/actstat/config.yml
```

in the chezmoi source repo, with exactly the four sources above. No secrets/tokens are ever written to config.

## Authentication

Token discovery (no secrets in config files):

1. `ACTSTAT_GITHUB_TOKEN`
2. `GH_TOKEN`, then `GITHUB_TOKEN`
3. If none set and `gh` is installed/authenticated, shell out to `gh auth token`.
4. If still none, make unauthenticated requests and emit a clear rate-limit/auth warning to stderr (private repos and
   org expansion will be limited).

The local machine already has `gh` authenticated as `bbugyi200` with `repo` + `read:org` scopes, so the `gh auth token`
fallback works out of the box.

## GitHub API Behavior (verified against live API)

Endpoints and shapes were confirmed against the live GitHub API during planning:

- **Org expansion:** `GET /orgs/{org}/repos?per_page=100` (paginated, follow until exhausted). Verified: `sase-org` → 9
  repos, `bobs-org` → 3 repos.
- **Completed runs:** `GET /repos/{owner}/{repo}/actions/runs?status=completed&per_page={limit}` → `workflow_runs[]`,
  each with `name`, `display_title`, `conclusion`, `event`, `head_branch`, `head_sha`, `run_number`, `html_url`,
  `created_at`, `run_started_at`, `updated_at`.
- **Failed-run detail:** for any run whose `conclusion != "success"`, `GET /repos/{owner}/{repo}/actions/runs/{id}/jobs`
  → `jobs[]` with `name`, `conclusion`, `html_url`, `steps[]` (`name`, `number`, `conclusion`). Verified that failed
  jobs expose failed steps (e.g. job `test (3.14)` → failed step `Run tests`).

Conclusion handling:

- `success` → passing.
- `failure`, `timed_out`, `cancelled`, `action_required`, `startup_failure`, and any other non-null conclusion →
  non-successful (each rendered with its specific label/icon).
- Compute duration from `run_started_at`/`created_at` → `updated_at`.

Resilience:

- Bounded concurrency across repositories (`--concurrency`).
- Retry with backoff on transient `5xx`/`429`/connection errors.
- Explicit handling for `401`/`403` (auth, rate-limit, abuse-limit), `404`, empty repos, disabled Actions, archived
  repos — each becomes a **per-repository error record**, never a global abort.

## Technical Direction (Rust)

- **Crate:** Cargo binary crate `actstat`, Rust 2021/2024 edition. Library + thin binary split (`src/lib.rs` +
  `src/main.rs`) so logic is unit-testable.
- **CLI:** `clap` v4 (derive) with the default-subcommand pattern described above.
- **Async runtime + HTTP:** `tokio` + `reqwest` (rustls) for concurrent, I/O-bound fan-out across repositories; bound
  concurrency with `futures::stream::buffer_unordered`.
- **Serialization:** `serde` + `serde_json` for JSON/JSONL; a serde-compatible YAML parser (`serde_yaml`, or its
  maintained fork `serde_norway`/`serde_yml`) for config.
- **Errors:** `anyhow` at the binary boundary; `thiserror` for typed config/domain errors and per-repo error records.
- **Time:** `jiff` (or `chrono`) to parse RFC3339 timestamps, compute durations, and render relative "x ago" times.
- **Terminal styling:** `owo-colors` + `anstream` for adaptive, NO_COLOR-aware, TTY-aware output; `terminal_size` for
  width-aware wrapping. Human rendering must read cleanly with color stripped.
- **Testing:** unit tests on pure functions (config parse/validate, source resolution/dedup, run/job normalization,
  conclusion classification, renderers via fixture JSON); a mock HTTP layer (e.g. `wiremock` or an injectable client
  trait) so no test needs real GitHub credentials or network.
- **Dev ergonomics:** a `Justfile` (or documented `cargo` commands) for `build`, `test`, `fmt`, `clippy`, `run`.
- **Architecture:** a single normalized result model
  (`Report -> [RepoReport] -> [RunReport] -> [JobReport] -> [StepReport]`) that **all** output formats render from, so
  GitHub parsing lives in exactly one place and never gets duplicated per format.

---

## Phase 1 — Project Skeleton & CLI Contract

**Objective:** Turn the skeleton repo into an installable Rust CLI with the correct command shape and flags, no network.

**Deliverables:**

- `Cargo.toml` with metadata, the binary `actstat`, and the dependency set above.
- `src/main.rs` (thin) + `src/lib.rs` (logic); module stubs (`cli`, `config`, `github`, `model`, `render`).
- `clap` CLI implementing the full `list` contract (all flags above) with the default-subcommand behavior (`actstat` ≡
  `actstat list`, and `actstat -n 3` ≡ `actstat list -n 3`).
- `list` produces a placeholder/stub `Report` (no network) so all three formats can be exercised end-to-end.
- Tests: command dispatch, default-command equivalence, flag parsing/validation (e.g. `--limit 0` rejected, bad
  `--format` rejected), and that each `--format` selects the right renderer.
- `Justfile` (or documented `cargo` commands) and a `.gitignore` for `target/`.

**Acceptance:**

- `cargo run -- --help`, `cargo run -- list --help` work and document the contract.
- `actstat`, `actstat list`, and `actstat -n 3` reach the same code path.
- `cargo test`, `cargo fmt --check`, `cargo clippy` pass; no network required.

## Phase 2 — Configuration Model & Chezmoi Config

**Objective:** Implement config discovery, parsing, validation, source-resolution primitives, and ship the real config.

**Deliverables:**

- Config discovery in the documented order (`--config` → `ACTSTAT_CONFIG` → `$XDG_CONFIG_HOME` → `~/.config`).
- `serde`-based parsing + validation of the `projects:` schema (org entries and repo entries), with clear, actionable
  errors for: missing/empty `projects`, malformed `owner/name`, an entry that is neither `org` nor `repo`, and invalid
  limits.
- Pure source-resolution logic: combine explicit repos + (later org-expanded) repos into a de-duplicated, sorted list of
  `owner/name`. (Org expansion call itself lands in Phase 3; the dedup/merge/sort logic and its tests land here.)
- The chezmoi source file `home/dot_config/actstat/config.yml` with the four required sources (sase-org, bobs-org,
  bbugyi200/dotfiles, bbugyi200/actstat). **This is the one intentional change outside the actstat repo.**
- README/docs note on applying via `chezmoi apply` and overriding the path with `ACTSTAT_CONFIG`/`--config` for tests.

**Acceptance:**

- Tests cover discovery/XDG fallback using isolated temp dirs and env overrides.
- Tests cover the exact desired config shape parsing into the expected model.
- Tests cover dedup/sort and validation errors.
- The chezmoi config source exists and contains no secrets.

## Phase 3 — GitHub Client, Auth & Org Expansion

**Objective:** Reliable, testable, failure-tolerant GitHub data access; expand orgs into concrete repositories.

**Deliverables:**

- A GitHub client behind a trait (or injectable transport) so tests use mock HTTP, not the network.
- Token discovery (env → `gh auth token` → unauthenticated-with-warning).
- Org repository expansion with full pagination and cross-source de-duplication into the Phase-2 resolution logic.
- Bounded concurrency (`--concurrency`) for repo-level requests; retry/backoff for transient `5xx`/`429`/connection
  errors; explicit mapping of `401`/`403`/`404`/rate-limit/abuse-limit to per-repository error records.
- Tests (mocked HTTP) for: pagination, duplicate collapse, auth errors, rate limits, and partial failures isolating to
  single repos.

**Acceptance:**

- Org sources expand into stable `owner/name` lists; one failing repo never aborts the run.
- Per-repo errors surface in JSON/JSONL and human output.
- No test requires real GitHub credentials or network.

## Phase 4 — Workflow Run & Failure-Detail Collection

**Objective:** Fetch the most recent completed runs per repo and enrich non-successful runs with useful job/step detail.

**Deliverables:**

- Completed-run fetch using `status=completed` honoring `-n/--limit` per repository.
- Normalize runs into the internal `RunReport` model (workflow name, title, number, event, branch, short SHA,
  conclusion, timestamps, computed duration, URL).
- For non-successful runs, fetch jobs and attach only non-successful jobs and their failed steps (`JobReport` /
  `StepReport`), preserving job/step URLs.
- Per-repo "no completed runs" handled as a neutral status.
- Tests (fixtures/mocks) for: pass, failure-with-failed-steps, cancelled, timed_out, no-runs, and malformed/partial
  payloads.

**Acceptance:**

- `actstat list -n 1 -f json` returns stable structured data for mocked workflows.
- Successful runs carry no job expansion; failed runs include failed job names, conclusions, failed step names, and
  URLs.
- Limit is per-repository.

## Phase 5 — Human & Machine Output Polish

**Objective:** Make it genuinely beautiful in a terminal and dependable in automation, from one result model.

**Deliverables:**

- Human renderer: grouped by repo, one compact colorized line per passing run (icon + workflow + branch + relative
  time), expanded tree for failures (failed jobs → failed steps → URLs), and clear neutral/error rows. Must read well
  with `--color never`/`NO_COLOR` and at common terminal widths.
- `json` and `jsonl` renderers from the same normalized model; `json` = one document with metadata + repos; `jsonl` =
  one line per run plus one per repo-error, each carrying its `repo`.
- Wire `--only-failures`, `--color`, `--fail-on-failure`, and the exit-code table.
- Snapshot-style tests for representative human output; schema/shape tests for JSON & JSONL (deterministic for
  deterministic input).

**Acceptance:**

- Human output is readable and attractive at common widths, color and no-color.
- JSON is deterministic; JSONL is one record per line; stdout stays pipe-clean.
- Exit codes behave per the table (incl. `--fail-on-failure`).

## Phase 6 — Documentation, Packaging & Final Verification

**Objective:** Ship with an excellent README and a complete verification pass.

**Deliverables:**

- Replace the empty `README.md` with a strong, approachable README covering: what `actstat` is and why; install & local
  dev (`cargo install --path .`); config location/schema with the real configured example; auth behavior; full
  `actstat list` usage; annotated human-output example (pass + fail); JSON & JSONL examples; exit-code table; cronjob
  recipe; troubleshooting (rate limits, private repos, missing config, no runs, disabled Actions).
- Concise contributor/dev section; ensure every command shown matches real CLI behavior.
- Run `cargo fmt`, `cargo clippy`, `cargo test`; do one real smoke run against the configured repos with `-n 1` if
  credentials are available, otherwise document that only mocked tests ran.

**Acceptance:**

- README is accurate, complete, and approachable; documented install/run path works.
- fmt/clippy/test all pass.
- The configured sources (sase-org, bobs-org, bbugyi200/dotfiles, bbugyi200/actstat) work without code changes.

## Cross-Phase Guardrails

- Keep all implementation changes inside the actstat repo **except** the single chezmoi config source in Phase 2.
- Never read or write secrets into files; tokens come only from env/`gh`.
- Keep all network access behind a testable abstraction; no test may require real credentials or network.
- One normalized result model feeds every output format — never duplicate GitHub-parsing logic per format.
- Make partial failure visible without hiding successful repositories.
- Keep the default command beautiful and quiet; push diagnostics behind verbosity flags and onto stderr.
