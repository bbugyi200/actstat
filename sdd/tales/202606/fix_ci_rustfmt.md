---
create_time: 2026-06-29 10:04:01
status: done
prompt: sdd/prompts/202606/fix_ci_rustfmt.md
---
# Plan: Fix failing CI `lint` job (rustfmt config drift)

## Problem

The GitHub Actions **CI** workflow for `bbugyi200/actstat` is failing on `master`. Only the `lint` job fails; `test` and
`msrv` pass.

The `lint` job runs `just lint`, whose first step is `cargo fmt --check`. That step exits non-zero with ~97 formatting
diffs, all of the form "rustfmt wants to collapse a wrapped expression onto a single line" — e.g.:

```
Diff in src/cli.rs:147:
-            std::env::var_os("NO_COLOR").is_none()
-                && std::io::stdout().is_terminal()
+            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
```

## Root cause

The repository ships **no project-local `rustfmt.toml`**. Because of that, formatting is non-deterministic across
environments:

- **Developer machine:** rustfmt finds no project config and falls back to the user's _global_ config at
  `~/.config/rustfmt/rustfmt.toml`, which sets `max_width = 80`. The actstat source was therefore formatted to **80
  columns**.
- **CI runner:** there is no global config and no project config, so rustfmt uses its built-in default
  `max_width = 100`. `cargo fmt --check` then sees the 80-column wrapping as "over-wrapped" relative to 100 columns and
  reports a diff for every such line — exit 1 → the `lint` job fails.

This was verified directly. Running the formatter against the committed source:

- `cargo fmt --check -- --config max_width=100` → **exit 1**, 97 diff hunks (reproduces CI exactly, same
  `src/cli.rs:147/211/234/...` locations as the CI log).
- `cargo fmt --check -- --config max_width=80` → **exit 0** (clean).

`max_width` is the _only_ setting that matters here. The global config also contains many nightly-only options, but CI
uses the **stable** toolchain (`dtolnay/rust-toolchain@stable`), which silently ignores every unstable option; and every
_stable_ option in the global config other than `max_width` is already at its rustfmt default. So pinning
`max_width = 80` fully reproduces the intended formatting on the stable toolchain.

This is a "works on my machine" config-drift bug: the formatting rules the code was written against live only in a
developer's home directory, not in the repo.

## Fix

Commit a small project-local `rustfmt.toml` at the repository root that pins the formatting rules the codebase was
actually written against, so `cargo fmt` produces identical results locally and in CI.

Minimal, intent-preserving content (matches the current code → zero source churn):

```toml
# Pin formatting so `cargo fmt` is reproducible across environments (incl. CI).
# The codebase is formatted to 80 columns; without this file CI falls back to
# rustfmt's default of 100 and `cargo fmt --check` fails. See the CI `lint` job.
max_width = 80
```

Rationale for `80` (rather than reformatting everything to rustfmt's default 100):

- It matches the existing, reviewed source and the author's established preference, so the fix is one new file with **no
  churn** to `src/`.
- Choosing 100 would require reformatting the entire crate _and_ still committing a `rustfmt.toml` to override the
  developer's global 80 (otherwise the next local `cargo fmt` would silently revert it). More churn, no benefit.

Notes:

- The crate is `edition = "2021"` (Cargo.toml). `cargo fmt` passes the crate edition to rustfmt, so the config does not
  need to set `edition`.
- Keep the file minimal: only `max_width`. Adding default-valued stable options is noise, and copying the global
  config's nightly-only options would spew harmless-but-confusing warnings into the CI log on the stable toolchain.

## Files changed

- **`rustfmt.toml`** (new, repo root) — pins `max_width = 80` with an explanatory comment.

No changes to `src/`, `Justfile`, `Cargo.toml`, or `.github/workflows/ci.yml`.

## Verification

1. `cargo fmt --check` → exits 0 (clean) with the new project config present.
2. `just lint` → passes end-to-end (`cargo fmt --check` **and** `cargo clippy --all-targets -- -D warnings`).
3. `just test` → still passes (no source change).
4. After merge to `master`, confirm the GitHub Actions **CI** run is green (`lint`, `test`, `msrv`) via `actstat` / the
   run page.

## Out of scope

- The developer's global `~/.config/rustfmt/rustfmt.toml` is left untouched; it is not part of this repo and will simply
  be overridden by the new project config when working in actstat.
- No reformatting of source to a different column width.
