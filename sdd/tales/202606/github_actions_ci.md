---
create_time: 2026-06-29 09:46:32
status: wip
prompt: sdd/prompts/202606/github_actions_ci.md
---
# Plan: GitHub Actions CI for lint + test (`just lint` / `just test`)

## Problem / motivation

`actstat` has a clean local quality gate (`just check` = `fmt-check` + `clippy` + `test`) and a green baseline (115
tests pass, clippy clean, `cargo fmt --check` clean), but **there is no CI**. The repository has no `.github/`
directory, so nothing enforces formatting, lints, or tests on push or pull request. Regressions can land on `master`
unnoticed, and contributors get no automated feedback.

The user wants **great GitHub Actions CI** that runs the project's own canonical commands — `just lint` and `just test`
— so CI and local development share exactly one definition of "lint" and "test". The Justfile is the single source of
truth; CI is a thin runner over it.

### Gap discovered during exploration

The Justfile currently exposes `fmt-check`, `fmt`, `clippy`, `test`, and `check` (= `fmt-check` + `clippy` + `test`),
but **there is no `lint` recipe**. So `just lint` does not exist yet and must be added before CI can call it. "Lint" in
this project naturally means "static checks that don't run tests": formatting verification plus clippy.

## Goals

- A `just lint` recipe that runs the static quality checks (`fmt-check` + `clippy`), distinct from `just test`.
- A GitHub Actions workflow that runs `just lint` and `just test` on every push to `master` and every pull request.
- "Great" in the sense of: fast (cached), reproducible, least-privilege, no wasted runs (concurrency cancellation),
  reliable (no network dependency — tests use a local wiremock mock), and honoring the declared MSRV.
- CI and local stay in lockstep: the workflow shells out to `just`, so changing a recipe changes both at once.

## Non-goals

- No release/publishing automation (crates.io, binary artifacts, `release-please`). CI here is lint + test only.
- No coverage reporting, fuzzing, or benchmarking pipelines.
- No change to application behavior or test logic; the baseline is already green and stays green.
- No auto-formatting or auto-fix commits from CI (CI verifies; it does not mutate the tree).

## Design decisions

1. **CI runs `just`, not raw cargo.** The workflow installs `just` and invokes `just lint` / `just test`. This keeps the
   Justfile as the one source of truth and prevents CI drift from local commands.

2. **Add a `lint` recipe; refactor `check` to reuse it.** Define `lint: fmt-check clippy`, then redefine
   `check: lint test` so the full local gate stays DRY and identical to "lint + test". `fmt-check`, `fmt`, `clippy`, and
   `test` are unchanged.

3. **Two parallel jobs: `lint` and `test`.** Separate jobs give independent, clearly-labeled status checks and run
   concurrently for faster feedback. `lint` needs the `rustfmt` and `clippy` components; `test` needs neither.

4. **Stable toolchain as the primary matrix; MSRV as a guard job.** Build/test on `stable`. Add a dedicated job that
   runs on Rust `1.85` (the `rust-version` declared in `Cargo.toml`) so the advertised MSRV is actually enforced and
   cannot silently rot. The MSRV job runs `cargo build`/`cargo test` (or `just test`) — it intentionally does **not**
   gate on clippy/fmt, since lint output varies by toolchain version.

5. **No system dependencies needed.** `reqwest` uses `rustls-tls` (no OpenSSL), and tests use `wiremock` (an in-process
   mock HTTP server), so CI needs **no apt packages and no network access** at test time. This makes runs fast and
   non-flaky.

6. **Repository pins no rustfmt config.** The local machine has a global `~/.rustfmt.toml` with nightly-only options,
   but the repo itself ships no `rustfmt.toml`, so both local stable runs and CI use the same stable defaults. CI
   formatting will therefore match local results. (No action required, but worth recording so a future "formatting
   differs in CI" surprise is pre-empted.)

7. **Least privilege + no wasted work.** `permissions: contents: read` only. A `concurrency` group keyed on the ref with
   `cancel-in-progress: true` so superseded pushes/PR updates stop their stale runs.

8. **Cache aggressively.** Use `Swatinem/rust-cache` so the registry, git deps, and `target/` are cached per job,
   turning warm runs into near-instant ones.

## Changes

### 1. `Justfile` — add `lint`, refactor `check`

Add a `lint` recipe and make `check` reuse it:

```just
# Lint: format check + clippy (no tests). Used by CI (`just lint`).
lint: fmt-check clippy

# Full pre-commit gate: lint + test.
check: lint test
```

`fmt-check`, `fmt`, `clippy`, and `test` remain exactly as they are. Net effect: `just lint` is the static-check half,
`just test` is the dynamic half, and `just check` is their union — the same total set of checks as today.

### 2. `.github/workflows/ci.yml` — new workflow

Reference design (final action versions/pins to be confirmed at implementation time; prefer pinning to known-good major
tags, and consider commit-SHA pinning for supply-chain hardening):

```yaml
name: CI

on:
  push:
    branches: [master]
  pull_request:

permissions:
  contents: read

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  lint:
    name: lint (just lint)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@just
      - run: just lint

  test:
    name: test (just test)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@just
      - run: just test

  msrv:
    name: msrv (1.85 build + test)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85.0
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@just
      - run: just test
```

Rationale for the chosen actions:

- `dtolnay/rust-toolchain` — minimal, fast, component-aware toolchain installer; supports `@stable` and pinned versions
  (`@1.85.0`) for the MSRV job.
- `Swatinem/rust-cache` — the standard Rust caching action; keys on lockfile + toolchain automatically.
- `taiki-e/install-action@just` — installs a prebuilt `just` binary in seconds (vs. a slow `cargo install just`).

### 3. `README.md` — CI status badge (polish)

Add a CI badge under the title so the build state is visible at a glance:

```markdown
[![CI](https://github.com/bbugyi200/actstat/actions/workflows/ci.yml/badge.svg)](https://github.com/bbugyi200/actstat/actions/workflows/ci.yml)
```

## Optional enhancements (decide at implementation time)

- **Cross-platform matrix.** The CLI is portable (rustls, no OS-specific code), so a `strategy.matrix.os` of
  `ubuntu-latest`, `macos-latest`, and `windows-latest` for the `test` job would catch platform regressions. Adds
  cost/time; recommended only if multi-OS support is a real goal. Default plan: Linux only.
- **`cargo doc` / doc-link check.** A `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` step catches broken intra-doc
  links. Could fold into the `lint` job or a `docs` recipe.
- **Dependabot / `cargo update` weekly job** to keep `Cargo.lock` and actions current. Out of the core scope but a
  natural follow-up for "great" CI hygiene.

## Verification / acceptance criteria

1. `just lint` runs locally and passes (it must be equivalent to today's `fmt-check` + `clippy`).
2. `just check` still runs the full gate (lint + test) and passes.
3. `just --list` shows the new `lint` recipe with its doc comment.
4. The workflow file is valid YAML and parses as a GitHub Actions workflow (e.g. `actionlint` if available, or a
   successful first run on push).
5. On push/PR, the `lint`, `test`, and `msrv` jobs all go green on the current baseline (already verified locally: 115
   tests pass, clippy clean, fmt clean).
6. CI requires no secrets, no network at test time, and no system packages.

## Risks and mitigations

- **`just` not present on runners.** Mitigated by explicitly installing it via `taiki-e/install-action@just`.
- **MSRV (1.85) build break against newer deps.** The dedicated `msrv` job surfaces this immediately; if a dependency
  raises its own MSRV beyond 1.85, the fix is to bump `rust-version` (and the job's pinned toolchain) or pin the dep. If
  MSRV enforcement proves noisy, the `msrv` job can be dropped without affecting `lint`/`test`.
- **Action supply-chain risk.** Mitigated by pinning action versions (and optionally commit SHAs) and granting only
  `contents: read`.
- **Formatting drift between local and CI.** Pre-empted: the repo ships no `rustfmt.toml`, so stable defaults apply
  identically in both places.

## Out of scope

- Publishing, releases, binary artifacts, container images.
- Code coverage, benchmarking, fuzzing.
- Any change to `src/` application or test code.
