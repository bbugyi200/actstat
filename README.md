# actstat

A fast, beautiful Rust CLI that reports the status of the most recent settled
default-branch GitHub Actions commits across a configured set of repositories.

`actstat` answers one question quickly: **which of my projects are healthy, and
what broke most recently?** A green commit collapses to a single compact line; a
red commit expands into every failed workflow run, failed job, and failed step
with direct links to the broken logs. Output is grouped by repository, quiet
about success, and loud and detailed about failure.

It is built to be comfortable as an interactive terminal command **and**
dependable inside cronjobs and scripts: async fan-out across repositories, a
small and conservative number of GitHub API calls, partial-failure isolation (one
broken repository never aborts the run), and stable machine-readable output
(`json` / `jsonl`) that keeps stdout pipe-clean.

## Highlights

- **Compact when healthy, detailed when not.** Green commits are one line; red
  commits expand to failed runs → failed jobs → failed steps → GitHub URLs.
- **Resilient.** A repo with no access, disabled Actions, or a rate limit becomes
  an inline error row instead of crashing the run.
- **Scriptable.** `--format json` / `--format jsonl` emit stable, deterministic
  records on stdout; all diagnostics go to stderr.
- **Conservative & fast.** Bounded-concurrency async HTTP with retry/backoff on
  transient errors.
- **No secrets in config.** Tokens come only from the environment or `gh`.

## Installation

`actstat` is a standard Cargo binary. Install it from a checkout of this repo:

```sh
cargo install --path .
```

This builds an optimized release binary and places `actstat` on your Cargo bin
path (typically `~/.cargo/bin`). Make sure that directory is on your `PATH`.

Requires a recent stable Rust toolchain (edition 2021; see `rust-version` in
[`Cargo.toml`](Cargo.toml) for the minimum supported version).

### Local development build

```sh
cargo build              # debug binary at target/debug/actstat
cargo build --release    # optimized binary at target/release/actstat
cargo run -- list -n 3   # build and run in one step
```

If you have [`just`](https://github.com/casey/just) installed, the
[`Justfile`](Justfile) wraps the common tasks (`just build`, `just run -- …`,
`just test`, `just check`).

## Configuration

`actstat` reads a small YAML config that lists the **projects** to inspect. Each
entry is either an entire **org** (expanded to all of its repositories) or a
single **repo** (`owner/name`):

```yaml
# ~/.config/actstat/config.yml
projects:
  - org: sase-org # all repositories in the sase-org organization
  - org: bobs-org # all repositories in the bobs-org organization
  - repo: bbugyi200/dotfiles # a single repository
  - repo: bbugyi200/actstat # this repository
```

All sources resolve into a de-duplicated, alphabetically sorted list of
`owner/name`. A repository named both explicitly and via its org appears once.

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

### Managing the config with chezmoi

The real machine config is managed by chezmoi. Its source lives at
`home/dot_config/actstat/config.yml` in the chezmoi source repo (the `home/`
source root is set by `.chezmoiroot`), so it materializes at
`~/.config/actstat/config.yml`:

```sh
chezmoi apply ~/.config/actstat/config.yml
```

### Overriding the config (tests / ad-hoc runs)

Point `actstat` at a different file without touching the managed config:

```sh
ACTSTAT_CONFIG=/tmp/actstat-test.yml actstat list
actstat list --config ./fixtures/config.yml
```

## Authentication

No secrets ever go in the config. `actstat` discovers a GitHub token in this
order:

1. `ACTSTAT_GITHUB_TOKEN`
2. `GH_TOKEN`, then `GITHUB_TOKEN`
3. `gh auth token` (if the [GitHub CLI](https://cli.github.com/) is installed and
   authenticated)
4. otherwise it makes **unauthenticated** requests and prints a warning to stderr

Unauthenticated requests have very low rate limits and cannot see private repos
or fully expand orgs, so an authenticated token is strongly recommended. The
simplest path is `gh auth login` with `repo` and `read:org` scopes — `actstat`
then picks up the token automatically via `gh auth token`.

## Usage

```
actstat [OPTIONS]          # same as `actstat list`
actstat list [OPTIONS]
```

Running `actstat` with no subcommand behaves exactly like `actstat list`, and the
`list` options work at the top level too (so `actstat -n 3` == `actstat list
-n 3`).

### `list` options

| Option | Description | Default |
| --- | --- | --- |
| `-n, --limit <N>` | Most-recent settled commits to inspect **per repository** (must be ≥ 1). | `1` |
| `-f, --format <human\|json\|jsonl>` | Output format. | `human` |
| `-c, --config <PATH>` | Explicit config path (overrides discovery). | discovery |
| `--color <auto\|always\|never>` | Color control; also honors `NO_COLOR`. | `auto` |
| `--only-failures` | Show only red commits in **human** output. | off |
| `--repo <OWNER/NAME>` | Restrict to a subset of configured repos (repeatable). | all |
| `--concurrency <N>` | Max repositories fetched concurrently. | `8` |
| `--fail-on-failure` | Exit non-zero if any inspected commit is red. | off |
| `-v, --verbose` | Increase diagnostic verbosity (stderr only; repeatable). | off |
| `-q, --quiet` | Suppress non-error diagnostics (stderr only). | off |

`--only-failures` filters the **human** view only; `json` and `jsonl` always
include every shown commit so machine consumers can do their own filtering.

### Examples

```sh
actstat                              # status of the latest settled commit per repo
actstat -n 5                         # inspect the 5 most recent settled commits each
actstat --only-failures              # show only what's broken
actstat --repo bbugyi200/actstat     # just one repo, ignoring the rest of the config
actstat -f json | jq '.repositories[] | select(.commits[].conclusion != "success")'
actstat --fail-on-failure -q         # quiet gate for cron/CI (see exit codes below)
```

## Output

### Human (default)

Repositories are grouped and sorted alphabetically; commits are newest settled
first on each repository's default branch. A green commit is one compact line
(icon · short SHA · title · branch · duration · relative time). A red commit
keeps that summary, appends its aggregate conclusion label, and expands into the
failed workflow runs, their failed jobs, failed steps, and direct GitHub URLs.
Repositories with no settled commits are omitted; repositories that error still
show a clear error row.

```text
bbugyi200/actstat
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

- `bbugyi200/actstat` — latest settled `master` commit passed; its workflow
  group took `2m30s` and finished `7m ago`.
- `bbugyi200/dotfiles` — latest settled `master` commit is red because `CI`
  failed; `Deploy Docs` passed on the same commit, so the commit shows
  `2 workflows`. The failed job `test (3.14)` failed at `step 5: Run tests`,
  with links straight to the job log and the run.
- `bobs-org/locked` — an error row: the token can't access it.

Color is adaptive: on by default when stdout is a TTY, off when piped, when
`--color never` is set, or when `NO_COLOR` is present. With color stripped the
layout is unchanged and byte-clean (no escape codes), so it diffs and greps
cleanly.

### JSON (`--format json`)

A single pretty-printed document: top-level metadata plus a `repositories` array.
Each repo carries its `commits` (and an `error` field only when it failed); each
commit carries its `runs`, and each problem run carries failed `jobs` and failed
`steps`. Output is deterministic for deterministic input.

```json
{
  "generated_at": "2026-06-29T12:00:00Z",
  "limit": 1,
  "repositories": [
    {
      "repo": "bbugyi200/actstat",
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
      "commits": [],
      "error": "403 Forbidden (token lacks access)"
    }
  ]
}
```

### JSONL (`--format jsonl`)

One JSON record per line for easy `jq`/shell piping. Every line carries a `type`
(`commit` or `repo_error`) and the `repo` it belongs to: one `commit` record per
shown commit, plus one `repo_error` record per errored repository.

```jsonl
{"branch":"master","conclusion":"success","duration_seconds":150,"event":"push","finished_at":"2026-06-29T11:52:30Z","repo":"bbugyi200/actstat","runs":[{"branch":"master","conclusion":"success","created_at":"2026-06-29T11:50:00Z","duration_seconds":150,"event":"push","jobs":[],"run_number":42,"sha":"a1b2c3d","title":"Add list subcommand","updated_at":"2026-06-29T11:52:30Z","url":"https://github.com/bbugyi200/actstat/actions/runs/1001","workflow":"CI"}],"sha":"a1b2c3d","title":"Add list subcommand","type":"commit","url":"https://github.com/bbugyi200/actstat/commit/a1b2c3d4e5f67890"}
{"branch":"master","conclusion":"failure","duration_seconds":250,"event":"push","finished_at":"2026-06-29T11:44:10Z","repo":"bbugyi200/dotfiles","runs":[{"branch":"master","conclusion":"failure","created_at":"2026-06-29T11:40:00Z","duration_seconds":250,"event":"push","jobs":[{"conclusion":"failure","name":"test (3.14)","steps":[{"conclusion":"failure","name":"Run tests","number":5}],"url":"https://github.com/bbugyi200/dotfiles/actions/runs/2002/job/3003"}],"run_number":128,"sha":"9f8e7d6","title":"Refactor shell init","updated_at":"2026-06-29T11:44:10Z","url":"https://github.com/bbugyi200/dotfiles/actions/runs/2002","workflow":"CI"},{"branch":"master","conclusion":"success","created_at":"2026-06-29T11:41:00Z","duration_seconds":120,"event":"push","jobs":[],"run_number":33,"sha":"9f8e7d6","title":"Refactor shell init","updated_at":"2026-06-29T11:43:00Z","url":"https://github.com/bbugyi200/dotfiles/actions/runs/2003","workflow":"Deploy Docs"}],"sha":"9f8e7d6","title":"Refactor shell init","type":"commit","url":"https://github.com/bbugyi200/dotfiles/commit/9f8e7d6c5b4a3210"}
{"error":"403 Forbidden (token lacks access)","repo":"bobs-org/locked","type":"repo_error"}
```

For example, to list every repo with a red shown commit:

```sh
actstat -f jsonl | jq -r 'select(.type=="commit" and .conclusion!="success") | .repo'
```

## Exit codes

Machine output always goes to **stdout**; diagnostics, warnings, and progress go
to **stderr**, so `actstat list -f json` is always pipe-clean.

| Code | Meaning |
| --- | --- |
| `0` | Ran to completion. Commit conclusions are reported, **not** gated — a red commit alone does not change the exit code. |
| `1` | Operational error: no usable config, config parse error, every inspected repository errored, or a fatal client/runtime error. |
| `2` | `--fail-on-failure` is set and at least one inspected commit was red. Also returned for a usage error such as a malformed `--repo OWNER/NAME` value. |

By default `actstat` reports without gating (exit `0`); pass `--fail-on-failure`
to turn a red commit into a non-zero exit for cron/CI.

## Cronjob recipe

`actstat` is built for unattended use. A typical cron entry runs quietly, and you
act on the exit code or capture machine output. Cron has a minimal environment,
so use absolute paths and make the token available — either export one of the
token env vars or rely on `gh` being authenticated for that user.

```cron
# Every 15 minutes: ping a healthcheck only when all configured repos are green.
*/15 * * * * GH_TOKEN=ghp_xxx /home/bryan/.cargo/bin/actstat --fail-on-failure -q && curl -fsS https://hc-ping.com/your-uuid
```

```sh
# Snapshot machine output for later inspection / alerting.
*/15 * * * * /home/bryan/.cargo/bin/actstat -f jsonl > /var/log/actstat.jsonl 2>>/var/log/actstat.err
```

With `--fail-on-failure`, exit `2` means "something is red" and exit `1` means
"actstat itself couldn't run" (bad config, no network, total auth failure) — so a
monitor can distinguish a broken pipeline from a broken check.

## Troubleshooting

- **"no actstat config found"** — no config exists on any discovery path.
  `actstat` prints a minimal example; create `~/.config/actstat/config.yml` (or
  point at one with `--config` / `ACTSTAT_CONFIG`).
- **"invalid config"** — the YAML parsed but failed validation (e.g. an entry
  that is neither `org:` nor `repo:`, or a `repo` that isn't `owner/name`). The
  message names the offending file.
- **Rate limited / "making unauthenticated requests" warning** — no token was
  found. Unauthenticated GitHub limits are very low; authenticate via `gh auth
  login` or set `ACTSTAT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN`. Transient
  rate limits and `5xx`/connection errors are retried with backoff automatically.
- **Private repos missing, or orgs not fully expanded** — the token lacks scope.
  Use a token with `repo` (private repos) and `read:org` (org expansion).
- **A repository shows an error row (`403`/`404`)** — the token can't see it, or
  it doesn't exist. This is isolated per repository and never aborts the run; the
  rest of your projects still report.
- **A repo is missing from output** — it had no qualifying settled commits in
  the recent default-branch run window. Empty repos are omitted rather than shown
  as neutral rows.
- **The newest GitHub commit is absent** — it still has queued or in-progress
  workflow runs. `actstat` reports the most recent default-branch commit whose
  workflow runs have all completed.
- **Use `-v` to diagnose** — add `--verbose` to print the token source and how
  many repositories are being inspected (on stderr, so stdout stays clean).

## Contributing / development

The crate is split into a thin binary (`src/main.rs`) and a library (`src/lib.rs`)
so all logic is unit-testable without spawning a process or touching the network.
Every output format renders from one normalized result model
(`Report → RepoReport → CommitReport → RunReport → JobReport → StepReport`), so
GitHub-parsing logic lives in exactly one place. HTTP is mocked in tests
(`wiremock`); no test requires real credentials or network.

```sh
cargo fmt                                   # format
cargo clippy --all-targets -- -D warnings   # lint (warnings are errors)
cargo test                                  # run the test suite
```

Or, with `just`:

```sh
just check    # fmt-check + clippy + test (the full pre-commit gate)
```

## License

MIT. See [`Cargo.toml`](Cargo.toml).
