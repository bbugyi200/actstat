# actstat

[![CI](https://github.com/bbugyi200/actstat/actions/workflows/ci.yml/badge.svg)](https://github.com/bbugyi200/actstat/actions/workflows/ci.yml)

A fast, readable Rust CLI that shows, for each configured repository, the newest
running GitHub Actions workflow plus the health of recent settled commits.

`actstat` answers three questions quickly: **what is running, which projects are
healthy, and what needs attention?** In each repository, the newest running
workflow across all branches appears above settled default-branch history. A
green commit collapses to one compact line; a red commit expands into the
problem runs and their returned problem jobs and steps, with direct links to
GitHub.

It is built to be comfortable as an interactive terminal command **and**
dependable inside cronjobs and scripts: async fan-out across repositories, a
small and conservative number of GitHub API calls, partial-failure isolation (one
broken repository never aborts the run), and structured machine-readable output
(`json` / `jsonl`) that keeps stdout pipe-clean.

## Highlights

- **Compact when healthy, detailed when not.** Green commits are one line; red
  commits expand to problem runs → returned jobs → steps → GitHub URLs.
- **Live running workflow.** Each repository's most recently started
  `in_progress` workflow run is shown above settled commits with a direct link
  to the live run log.
- **Resilient.** Repository and organization API failures become inline error
  rows instead of aborting collection for other projects.
- **Scriptable.** `--format json` / `--format jsonl` emit structured records in
  consistent repository order on stdout; all diagnostics go to stderr.
- **Conservative & fast.** Bounded-concurrency async HTTP with retry/backoff on
  transient errors.
- **No secrets in config.** Tokens come only from the environment or `gh`.

## Installation

`actstat` requires Rust 1.85 or newer. Install the latest revision directly from
GitHub:

```sh
cargo install --git https://github.com/bbugyi200/actstat
```

Or install it from an existing checkout:

```sh
cargo install --path .
```

This builds an optimized release binary and places `actstat` on your Cargo bin
path (typically `~/.cargo/bin`). Make sure that directory is on your `PATH`.

The exact minimum supported Rust version is recorded in
[`Cargo.toml`](Cargo.toml).

## Configuration

`actstat` reads a YAML config whose `projects` list contains organization and
repository sources. Each entry must set exactly one of `org` or `repo`:

```yaml
# ~/.config/actstat/config.yml
projects:
  - org: example-org
    exclude:
      - example-org/sandbox
  - repo: octocat/Hello-World
```

An `org` entry expands to every repository visible to the current token, except
that archived repositories and forks are excluded by default. A `repo` entry
selects exactly one `owner/name`, including an archived repository or fork.
All sources are merged into a de-duplicated, alphabetically sorted list. A
repository selected under the same exact `owner/name` both explicitly and
through its organization appears once.

### Org filters

An `org:` entry can refine how that organization's repositories are expanded:

| Key | Description | Default |
| --- | --- | --- |
| `exclude` | List of `owner/name` repositories to drop from this org expansion. Each owner must match the org. An explicit `repo` entry can still add the repository. | `[]` |
| `include_archived` | Include archived repos in the org expansion. | `false` |
| `include_forks` | Include forked repos in the org expansion. | `false` |

These keys are valid only on `org` entries. Organization names must be bare
logins such as `example-org`; repository names must use `owner/name` form.

### Where the config is found

The config path is resolved in this order; the first hit wins:

1. `--config PATH`
2. `ACTSTAT_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/actstat/config.yml`
4. `~/.config/actstat/config.yml`

`--config` and `ACTSTAT_CONFIG` are explicit overrides and are used verbatim; the
two well-known locations are tried in order and the first existing file is used.
If none is found, `actstat` exits `1` with an actionable message and a minimal
example config printed to stderr.

### Ad-hoc configuration

Point `actstat` at a different file without touching the default config:

```sh
ACTSTAT_CONFIG=/tmp/actstat-test.yml actstat list
actstat list --config ./fixtures/config.yml
```

## Authentication

No secrets go in the YAML config. The easiest authentication setup is to install
the [GitHub CLI](https://cli.github.com/), run `gh auth login`, and let `actstat`
read the resulting token. Token discovery follows this order:

1. `ACTSTAT_GITHUB_TOKEN`
2. `GH_TOKEN`, then `GITHUB_TOKEN`
3. `gh auth token` (if the [GitHub CLI](https://cli.github.com/) is installed and
   authenticated)
4. otherwise it makes **unauthenticated** requests and prints a warning to stderr

Empty environment variables are ignored. Unauthenticated requests work for
public resources but have low rate limits and cannot discover private
repositories. For a fine-grained token, grant read access to the desired
repositories with [**Actions: read**](https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository)
and [**Metadata: read**](https://docs.github.com/en/rest/repos/repos#list-organization-repositories)
permissions. A classic personal access token needs the `repo` scope for private
repositories. Organization policy or SSO can impose additional access
requirements; verify that the token can see every configured repository.

## Usage

```
actstat [OPTIONS]          # same as `actstat list`
actstat list [OPTIONS]
```

Running `actstat` with no subcommand behaves exactly like `actstat list`, and the
`list` options work at the top level too (so `actstat -n 3` == `actstat list
-n 3`).

### What gets reported

`actstat` deliberately treats live work and settled history differently:

- **Running:** for each repository, across all branches, the single most
  recently started workflow whose GitHub status is `in_progress`. Queued,
  waiting, pending, requested, and completed runs are not part of this section.
  `--no-active` skips this per-repository lookup.
- **Settled:** for each repository, workflow runs from its default branch are
  grouped by commit SHA. If GitHub returns multiple runs for the same workflow
  and commit, `actstat` keeps the run with the highest run ID. A commit is
  eligible only when all retained runs are completed. Newer unsettled commits
  are skipped, so older settled commits can still be shown.
- **Health:** a settled commit is green when all its selected runs concluded
  `success`, `skipped`, or `neutral`. Conclusions such as `failure`,
  `cancelled`, `timed_out`, `action_required`, `startup_failure`, and `stale`
  make the commit red.

Each workflow-run lookup requests only the first GitHub API page, with up to 100
runs; `actstat` does not paginate into older workflow-run history. `--limit` is
applied after settled commits are selected and does not affect the running
section. For each problem run, job enrichment likewise uses one API page of up
to 100 jobs; every step returned inside those jobs is considered.

### `list` options

| Option | Description | Default |
| --- | --- | --- |
| `-n, --limit <N>` | Most-recent settled commits to inspect **per repository** (must be ≥ 1). | `1` |
| `-f, --format <human\|json\|jsonl>` | Output format. | `human` |
| `-c, --config <PATH>` | Explicit config path (overrides discovery). | discovery |
| `--color <auto\|always\|never>` | Color control; `auto` honors `NO_COLOR`. | `auto` |
| `--only-failures` | Show only errors and red settled commits in **human** output. | off |
| `--no-active` | Skip fetching and showing the currently running workflow run. | off |
| `--repo <OWNER/NAME>` | Filter the resolved repositories by exact `owner/name` (repeatable). | all |
| `--concurrency <N>` | Max org expansions or repository collections in flight; values below `1` behave as `1`. | `8` |
| `--fail-on-failure` | Exit `2` if any inspected settled commit is red. | off |
| `-v, --verbose` | Increase diagnostic verbosity (stderr only; repeatable). | off |
| `-q, --quiet` | Suppress non-error diagnostics (stderr only). | off |

`--only-failures` filters the human view only; it also hides the running section
because a running workflow is not yet a failure. JSON and JSONL are unaffected.
`--repo` is a filter, not a way to add a repository. Resolution happens first:
`actstat` expands every configured organization, merges those results with
explicit `repo` entries, and only then applies `--repo`. The flag therefore does
not avoid organization API calls, and an error from any configured organization
still appears in the output. Matching uses the exact resolved `owner/name`
string, including case; a well-formed name that was not resolved matches
nothing.

### Examples

```sh
actstat                              # status of the latest settled commit per repo
actstat -n 5                         # inspect the 5 most recent settled commits each
actstat --only-failures              # show only what's broken
actstat --no-active                  # skip running workflow lookup
actstat --repo bbugyi200/actstat     # select one resolved repo (orgs are still expanded)
actstat -f jsonl | jq -r 'select(.type == "active_commit") | .runs[].url'
actstat -f jsonl | jq -r 'select(.type == "commit" and .conclusion == "failure") | .repo'
actstat --fail-on-failure -q         # quiet gate for cron/CI (see exit codes below)
```

## Output

### Human (default)

Repositories are grouped and sorted alphabetically. For each repository, its
newest running workflow, if one exists, appears first with its branch, elapsed
time, status, and live run URL. Settled default-branch commits follow newest
first. A green commit is one compact line containing its short SHA and title,
plus branch, aggregate duration, and relative completion time when available. A
red commit keeps that summary and expands each problem run with its returned
problem jobs and steps, plus relevant GitHub URLs. Repositories with neither a
running workflow nor a settled commit are omitted; repository and organization
errors remain visible as red rows.

```text
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
```

Reading the example:

- `bbugyi200/actstat` — a newer `master` commit is running: `CI` has been
  running for `1m20s` and links to its live run log. Below it, the latest
  settled `master` commit passed; its workflow group took `2m30s` and finished
  `7m ago`.
- `bbugyi200/dotfiles` — latest settled `master` commit is red because `CI`
  failed; `Deploy Docs` passed on the same commit, so the commit shows
  `2 workflows`. The failed job `test (3.14)` failed at `step 5: Run tests`,
  with links straight to the job log and the run.
- `bobs-org/locked` — an error row: the token can't access it.

In `auto` mode, color is enabled only when stdout is a TTY and `NO_COLOR` is
absent. `--color always` and `--color never` override that automatic decision.
With color stripped, the layout is unchanged and byte-clean (no escape codes),
so it diffs and greps cleanly.

The commit-level red label is always `failure`; individual runs retain their
more specific conclusions such as `cancelled` or `timed_out`.

### JSON (`--format json`)

A single pretty-printed document: top-level metadata plus a `repositories` array.
Each repository always carries `active` and `commits` arrays, plus an `error`
field only when collection failed. `active` contains zero or one commit; when
present, that commit contains exactly one run with `status: "in_progress"`.
Settled commits contain their selected completed runs. Problem runs contain only
problem jobs, and those jobs contain only problem steps; healthy jobs and steps
are intentionally omitted. Optional duration fields are absent when GitHub did
not provide enough valid timestamps to calculate them.

```json
{
  "generated_at": "2026-06-29T12:00:00Z",
  "limit": 1,
  "repositories": [
    {
      "repo": "bbugyi200/actstat",
      "active": [
        {
          "sha": "f00ba12",
          "title": "Add progress spinner",
          "branch": "master",
          "event": "push",
          "url": "https://github.com/bbugyi200/actstat/commit/f00ba1234567890",
          "started_at": "2026-06-29T11:58:40Z",
          "runs": [
            {
              "workflow": "CI",
              "title": "Add progress spinner",
              "run_number": 44,
              "event": "push",
              "branch": "master",
              "sha": "f00ba12",
              "status": "in_progress",
              "url": "https://github.com/bbugyi200/actstat/actions/runs/1044",
              "created_at": "2026-06-29T11:58:35Z",
              "started_at": "2026-06-29T11:58:40Z"
            }
          ]
        }
      ],
      "commits": [
        {
          "sha": "a1b2c3d",
          "title": "Add list subcommand",
          "branch": "master",
          "event": "push",
          "conclusion": "success",
          "url": "https://github.com/bbugyi200/actstat/commit/a1b2c3d4e5f67890",
          "finished_at": "2026-06-29T11:52:30Z",
          "duration_seconds": 150,
          "runs": [
            {
              "workflow": "CI",
              "title": "Add list subcommand",
              "run_number": 42,
              "event": "push",
              "branch": "master",
              "sha": "a1b2c3d",
              "conclusion": "success",
              "url": "https://github.com/bbugyi200/actstat/actions/runs/1001",
              "created_at": "2026-06-29T11:50:00Z",
              "updated_at": "2026-06-29T11:52:30Z",
              "duration_seconds": 150,
              "jobs": []
            }
          ]
        }
      ]
    },
    {
      "repo": "bbugyi200/dotfiles",
      "active": [],
      "commits": [
        {
          "sha": "9f8e7d6",
          "title": "Refactor shell init",
          "branch": "master",
          "event": "push",
          "conclusion": "failure",
          "url": "https://github.com/bbugyi200/dotfiles/commit/9f8e7d6c5b4a3210",
          "finished_at": "2026-06-29T11:44:10Z",
          "duration_seconds": 250,
          "runs": [
            {
              "workflow": "CI",
              "title": "Refactor shell init",
              "run_number": 128,
              "event": "push",
              "branch": "master",
              "sha": "9f8e7d6",
              "conclusion": "failure",
              "url": "https://github.com/bbugyi200/dotfiles/actions/runs/2002",
              "created_at": "2026-06-29T11:40:00Z",
              "updated_at": "2026-06-29T11:44:10Z",
              "duration_seconds": 250,
              "jobs": [
                {
                  "name": "test (3.14)",
                  "conclusion": "failure",
                  "url": "https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003",
                  "steps": [
                    {
                      "name": "Run tests",
                      "number": 5,
                      "conclusion": "failure"
                    }
                  ]
                }
              ]
            },
            {
              "workflow": "Deploy Docs",
              "title": "Refactor shell init",
              "run_number": 33,
              "event": "push",
              "branch": "master",
              "sha": "9f8e7d6",
              "conclusion": "success",
              "url": "https://github.com/bbugyi200/dotfiles/actions/runs/2003",
              "created_at": "2026-06-29T11:41:00Z",
              "updated_at": "2026-06-29T11:43:00Z",
              "duration_seconds": 120,
              "jobs": []
            }
          ]
        }
      ]
    },
    {
      "repo": "bobs-org/locked",
      "active": [],
      "commits": [],
      "error": "403 Forbidden (token lacks access)"
    }
  ]
}
```

### JSONL (`--format jsonl`)

One JSON record per line for easy `jq`/shell piping. Every line carries a `type`
(`active_commit`, `commit`, or `repo_error`) and its `repo`: at most one
`active_commit` record per repository, one `commit` record per settled commit,
and one `repo_error` record per errored repository. Active records precede
settled records for the same repository.

```jsonl
{"branch":"master","event":"push","repo":"bbugyi200/actstat","runs":[{"branch":"master","created_at":"2026-06-29T11:58:35Z","event":"push","run_number":44,"sha":"f00ba12","started_at":"2026-06-29T11:58:40Z","status":"in_progress","title":"Add progress spinner","url":"https://github.com/bbugyi200/actstat/actions/runs/1044","workflow":"CI"}],"sha":"f00ba12","started_at":"2026-06-29T11:58:40Z","title":"Add progress spinner","type":"active_commit","url":"https://github.com/bbugyi200/actstat/commit/f00ba1234567890"}
{"branch":"master","conclusion":"success","duration_seconds":150,"event":"push","finished_at":"2026-06-29T11:52:30Z","repo":"bbugyi200/actstat","runs":[{"branch":"master","conclusion":"success","created_at":"2026-06-29T11:50:00Z","duration_seconds":150,"event":"push","jobs":[],"run_number":42,"sha":"a1b2c3d","title":"Add list subcommand","updated_at":"2026-06-29T11:52:30Z","url":"https://github.com/bbugyi200/actstat/actions/runs/1001","workflow":"CI"}],"sha":"a1b2c3d","title":"Add list subcommand","type":"commit","url":"https://github.com/bbugyi200/actstat/commit/a1b2c3d4e5f67890"}
{"branch":"master","conclusion":"failure","duration_seconds":250,"event":"push","finished_at":"2026-06-29T11:44:10Z","repo":"bbugyi200/dotfiles","runs":[{"branch":"master","conclusion":"failure","created_at":"2026-06-29T11:40:00Z","duration_seconds":250,"event":"push","jobs":[{"conclusion":"failure","name":"test (3.14)","steps":[{"conclusion":"failure","name":"Run tests","number":5}],"url":"https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003"}],"run_number":128,"sha":"9f8e7d6","title":"Refactor shell init","updated_at":"2026-06-29T11:44:10Z","url":"https://github.com/bbugyi200/dotfiles/actions/runs/2002","workflow":"CI"},{"branch":"master","conclusion":"success","created_at":"2026-06-29T11:41:00Z","duration_seconds":120,"event":"push","jobs":[],"run_number":33,"sha":"9f8e7d6","title":"Refactor shell init","updated_at":"2026-06-29T11:43:00Z","url":"https://github.com/bbugyi200/dotfiles/actions/runs/2003","workflow":"Deploy Docs"}],"sha":"9f8e7d6","title":"Refactor shell init","type":"commit","url":"https://github.com/bbugyi200/dotfiles/commit/9f8e7d6c5b4a3210"}
{"error":"403 Forbidden (token lacks access)","repo":"bobs-org/locked","type":"repo_error"}
```

## Exit codes

Machine output always goes to **stdout**; diagnostics, warnings, and progress go
to **stderr**, so `actstat list -f json` is always pipe-clean.

| Code | Meaning |
| --- | --- |
| `0` | Normal completion: the final report is empty or contains a non-error repository row, and either `--fail-on-failure` is off or no settled commit is red. |
| `1` | Operational error: no usable config, config parse error, a fatal client/runtime error, or a non-empty final report in which every row is a repository or organization error. |
| `2` | `--fail-on-failure` is set and at least one inspected settled commit is red, or the command line contains a usage error. |

By default `actstat` reports without gating (exit `0`); pass `--fail-on-failure`
to turn a red settled commit into a non-zero exit for cron/CI. A running
workflow has not failed or passed yet, so it never triggers exit `2`.

Per-repository and per-organization errors are represented in the output so one
inaccessible project does not abort the rest of the report. If those partial
errors must fail an automation, inspect `repo_error` JSONL records explicitly.
Successful repositories with neither a running workflow nor a settled commit
are omitted before the exit code is chosen. Consequently, an organization error
plus only empty successful repositories exits `1`, because the final report
contains error rows only.

## Cronjob recipe

`actstat` is built for unattended use. Cron has a minimal environment, so use
absolute paths for the binary, config, and output files. Make authentication
available to the cron process through its environment or an authenticated `gh`
installation that cron can find on `PATH`; do not put a real token directly in
a checked-in crontab.

```cron
# Every 15 minutes: write machine output and return 2 if a settled commit is red.
*/15 * * * * ACTSTAT_CONFIG=/home/you/.config/actstat/config.yml /home/you/.cargo/bin/actstat --fail-on-failure -q -f jsonl > /home/you/.local/state/actstat.jsonl 2>> /home/you/.local/state/actstat.err
```

With `--fail-on-failure`, exit `2` means "something is red" and exit `1` means
"actstat itself couldn't run" (bad config, no network, or a final report that
contains only error rows). Partial errors can still exit `0`, so an automation
that treats missing status as unhealthy should also reject `repo_error` records.

## Troubleshooting

- **"no actstat config found"** — no config exists on any discovery path.
  `actstat` prints a minimal example; create `~/.config/actstat/config.yml` (or
  point at one with `--config` / `ACTSTAT_CONFIG`).
- **"invalid config"** — the YAML parsed but failed validation (e.g. an entry
  that sets both/neither `org` and `repo`, a malformed `owner/name`, or an org
  filter attached to a `repo` entry). The message names the offending file.
- **Rate limited / "making unauthenticated requests" warning** — no token was
  found. Unauthenticated GitHub limits are very low; authenticate via `gh auth
  login` or set `ACTSTAT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN`. Transient
  rate limits and `5xx`/connection errors are retried with backoff automatically.
- **Private repos missing, or orgs not fully expanded** — verify that the token
  can access the repository and has Actions and Metadata read permissions.
  Organization policy or SSO may also restrict a token.
- **A repository shows an error row (`403`/`404`)** — the token can't see it, or
  it doesn't exist. This is isolated per repository and never stops collection
  for the rest of your projects. If the final output contains only error rows,
  the command exits `1`.
- **A repo is missing from output** — it had no currently running workflow run
  and no qualifying settled commits in the recent default-branch run window.
  Empty repos are omitted rather than shown as neutral rows, and
  `--only-failures` hides healthy repositories in human output. An org expansion
  also omits archived repositories and forks unless their filters enable them.
- **A completed feature-branch run is absent from settled history** — settled
  commits come only from each repository's default branch. The one running
  workflow may come from any branch.
- **A workflow is queued but not shown** — queued, waiting, pending, and
  requested runs are not considered active. Once GitHub marks a workflow run
  `in_progress`, the most recently started running workflow appears unless
  `--no-active` is used.
- **The newest GitHub commit is absent** — if it has no `in_progress` workflow
  run and no settled default-branch run yet, it is omitted. The running-run
  and default-branch lookups each inspect only the first API page, with at most
  100 returned runs.
- **`--repo` produces no output** — the flag only filters repositories selected
  by the config. Confirm the exact, case-sensitive `owner/name` was resolved
  directly or through a successful org expansion. All configured organizations
  are still expanded before this filter is applied.
- **Use `-v` to diagnose** — add `--verbose` to print the token source and how
  many repositories are being inspected (on stderr, so stdout stays clean).

## Contributing / development

The crate is split into a thin binary (`src/main.rs`) and a library (`src/lib.rs`)
so all logic is unit-testable without spawning a process or touching the network.
Every output format renders from one normalized result model
(`Report → RepoReport → ActiveCommitReport → ActiveRunReport` and
`Report → RepoReport → CommitReport → RunReport → JobReport → StepReport`), so
GitHub-parsing logic lives in exactly one place. HTTP is mocked in tests
(`wiremock`); no test requires real credentials or network.

```sh
cargo build                                 # debug build
cargo run -- list -n 3                      # build and run
cargo fmt --check                           # formatting
cargo clippy --all-targets -- -D warnings   # lint (warnings are errors)
cargo test                                  # run the test suite
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps  # validate Rust API docs
```

Or, with `just`:

```sh
just check    # fmt-check + clippy + test (the full pre-commit gate)
```

## License

MIT. See [`Cargo.toml`](Cargo.toml).
