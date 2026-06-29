---
create_time: 2026-06-29 12:53:42
status: done
prompt: sdd/prompts/202606/org_exclude_repos.md
---
# Plan: First-class "exclude repos from an org" configuration

## Goal

Let an `actstat` config include **all** of an organization's repositories while **excluding** a named subset — e.g.
include every `sase-org` repo _except_ `sase-org/sase-android`. Then update the chezmoi-managed config so the real
machine starts excluding `sase-org/sase-android`.

## Surprising current state (read this first)

The exclusion mechanism is **already implemented and live end-to-end** — it is just mislabeled as unfinished,
undocumented for users, and missing a few guardrails. Evidence:

- **Config schema already has it.** `OrgSource` carries `exclude: Vec<RepoName>` (plus `include_archived` /
  `include_forks`), and the raw deserializer (`RawProject`) and `validate_org()` already parse and validate `exclude`
  entries as `owner/name` repos. (`src/config.rs`)
- **The filter is already applied during org expansion.** `list_org_repos()` drops archived/forked repos and any repo in
  `exclude`: `….filter(|name| !org.exclude.contains(name))`. (`src/github.rs`, in `list_org_repos`)
- **It is wired into the real run path.** `cli.rs` calls `github::resolve_repositories(&client, &config, …)`, which
  calls `list_org_repos()` for every configured org and merges the (filtered) results with explicit repos via
  `resolve_repos()`. This is the actual production code path, not a stub.
- **It is already unit-tested.** `list_org_repos_applies_filters` asserts that archived, forked, and excluded entries
  are dropped; config tests assert `exclude` parses into `RepoName`s.

So the feature _works today_. What is wrong is the **framing and polish**:

1. **Stale "reserved / Phase 3 / forward-compatible" labeling.** Doc comments in `src/config.rs` and `src/github.rs`
   describe `exclude` / `include_archived` / `include_forks` as "Reserved for Phase 3 — parsed now so configs using it
   don't break." Phase 3 (org expansion) has shipped; these filters are live. The comments now misrepresent a working
   feature as unfinished.
2. **No user-facing documentation.** The README "Configuration" section only documents `org:` and `repo:`. A user has no
   way to discover `exclude` (or the include-flags) without reading the source.
3. **Two silent-misconfiguration footguns** (see "Validation hardening").

This plan turns `exclude` into a documented, validated, first-class feature and then flips it on for
`sase-org/sase-android` in the chezmoi config.

## Config shape (what the user writes)

`exclude` is a list of `owner/name` repositories attached to an `org:` entry. Keeping the full `owner/name` form (rather
than a bare repo name) matches how every other repository is referenced in the config and stays consistent with the
existing parser and tests:

```yaml
projects:
  - org: sase-org # all sase-org repos…
    exclude:
      - sase-org/sase-android # …except this one
  - repo: bbugyi200/actstat
```

Already-supported sibling flags on an `org:` entry (to be documented alongside `exclude`): `include_archived: true` and
`include_forks: true`, both defaulting to `false`.

## Scope

### A. De-stale the feature (documentation & comments only; no behavior change)

- **`src/config.rs`** — rewrite the doc comments that call `exclude` / `include_archived` / `include_forks` "reserved",
  "forward-compatible", or "Phase 3" so they describe live, supported org filters. Specifically the module-level doc,
  the `OrgSource` field docs, the `resolve_repos` doc, and `validate_org`'s "reserved filters" wording. Keep the
  separate, still-true note that genuinely _unknown_ future fields are tolerated (that forward-compat behavior stays —
  it is different from these now-shipped fields).
- **`src/github.rs`** — update `list_org_repos`'s doc comment to stop calling the filters "reserved".
- **Rename now-misleading test names** for clarity (behavior unchanged): `org_entry_defaults_reserved_filters` →
  `org_entry_defaults_filters`; `org_entry_parses_reserved_filters_forward_compatibly` → `org_entry_parses_org_filters`.
  Keep `unknown_future_fields_are_ignored_not_rejected` as-is — truly unknown fields are still tolerated.

### B. Document the feature for users (README)

In the README "Configuration" section, extend the example and add a short subsection documenting org filters. Show the
headline use case — include a whole org but drop one repo:

```yaml
projects:
  - org: sase-org
    exclude:
      - sase-org/sase-android # include every sase-org repo except this one
  - org: bobs-org
  - repo: bbugyi200/dotfiles
```

Document, in a small list or table:

- `exclude:` — list of `owner/name` repos to drop from that org's expansion. Entry owners must match the org (see
  hardening). A repo dropped here can still be inspected if it is _also_ listed explicitly via its own `repo:` entry
  (explicit + org sources are merged), so note that `exclude` removes a repo from _org expansion_, not from the run
  unconditionally.
- `include_archived:` (default `false`) — include archived repos in expansion.
- `include_forks:` (default `false`) — include forks in expansion.

Keep the built-in `example_config()` minimal (it is the "no config found" nudge); the README is the place for the richer
example.

### C. Validation & matching hardening (close the two footguns)

These are small, low-risk guardrails that make `exclude` behave the way a user expects. Each is independently scoped so
it can be trimmed in review.

1. **Reject org-only keys on a `repo:` entry.** Today `exclude`, `include_archived`, and `include_forks` on a `repo:`
   entry are silently ignored (the repo branch of `validate()` never reads them). Make a present org-only key on a
   `repo:` entry an actionable error, e.g.: `project #N: `exclude`only applies to`org:`entries, not`repo:``. (This is
   distinct from — and does not weaken — the existing tolerance of _unknown_ fields; these are known fields used in the
   wrong place.)

2. **Reject `exclude` entries whose owner isn't the org.** `/orgs/{org}/repos` only ever returns repos owned by that
   org, so an `exclude` entry like `other-org/x` under `org: sase-org` can never match — it is silently dead weight and
   almost certainly a typo. In `validate_org`, after parsing each `exclude` entry into a `RepoName`, require its owner
   to equal the org (compared case-insensitively, since GitHub logins are case-insensitive), else error:
   `project #N `exclude`: `other-org/x`is not in org`sase-org``. This preserves all existing tests (their exclude owners
   already match the org).

3. **Make the exclude match case-insensitive.** GitHub owners/repo names are case-insensitive, but the live filter uses
   `org.exclude.contains(name)` (exact, case-sensitive `RepoName` equality). Change _only the exclude filter_ in
   `list_org_repos` to compare owner and name with `eq_ignore_ascii_case`, so `sase-org/Sase-Android` in config still
   excludes `sase-org/sase-android` from the API. Do **not** change `RepoName`'s `Eq`/`Ord` — those drive
   dedup/sort/`--repo` matching elsewhere and must stay exact.

### D. Tests

Add/extend unit tests (mirroring existing style in `src/config.rs` and `src/github.rs`):

- `exclude`/`include_archived`/`include_forks` on a `repo:` entry → error mentioning the offending key and `org`.
- `exclude` entry with a non-matching owner → error mentioning the org.
- `exclude` entry whose owner matches the org but differs in case → accepted.
- A case-mismatched `exclude` entry actually filters the API repo in `list_org_repos` (extend the existing
  `list_org_repos_applies_filters` mock or add a sibling test).
- Existing tests (`org_entry_defaults_filters`, `org_entry_parses_org_filters`, `list_org_repos_applies_filters`,
  `malformed_exclude_entry_is_an_error`, `unknown_future_fields_are_ignored_not_rejected`) continue to pass.

### E. Flip it on in the chezmoi config (the concrete request)

In the **chezmoi source repo** (separate from the actstat repo), edit the actstat config at
`home/dot_config/actstat/config.yml` (materializes at `~/.config/actstat/config.yml`) so the `sase-org` org entry
excludes `sase-org/sase-android`:

```yaml
projects:
  - org: sase-org # all repositories in the sase-org organization…
    exclude:
      - sase-org/sase-android # …except this one
  - org: bobs-org # all repositories in the bobs-org organization
  - repo: bbugyi200/dotfiles # a single repository
  - repo: bbugyi200/actstat # this repository
```

This is a config-only edit in a different repository; it should land after the actstat feature work (so the config it
relies on is documented and validated). Apply with `chezmoi apply ~/.config/actstat/config.yml` if a live apply is
wanted.

## Files changed

- `src/config.rs` — de-stale doc comments; reject org-only keys on `repo:` entries; validate `exclude` owner == org
  (case-insensitive); rename two tests; new validation tests.
- `src/github.rs` — case-insensitive exclude match in `list_org_repos`; de-stale doc comment; test for case-insensitive
  exclusion.
- `README.md` — document `exclude` / `include_archived` / `include_forks` in the Configuration section with the
  include-all-but-one example.
- `home/dot_config/actstat/config.yml` (in the **chezmoi** source repo) — add the `exclude: [sase-org/sase-android]`
  filter under the `sase-org` entry.

## Verification

1. `cargo test` (or `just test`) — all unit tests pass, including the new validation and case-insensitive matching
   tests.
2. `just lint` — `cargo fmt --check` and `cargo clippy -D warnings` clean.
3. Sanity-check parsing against a temp config that excludes `sase-org/sase-android` (e.g. via `ACTSTAT_CONFIG`); confirm
   the run does not list `sase-org/sase-android` while still listing other `sase-org` repos (requires GitHub access;
   otherwise rely on the mocked `list_org_repos` test).
4. Confirm the rejection paths: a `repo:` entry with `exclude:` and an `exclude` entry with a foreign owner each produce
   a clear config error.
5. After the chezmoi edit, `chezmoi diff ~/.config/actstat/config.yml` shows only the added `exclude` block.

## Design decisions (and alternatives considered)

- **Keep `owner/name` form for `exclude` entries** rather than bare repo names. Bare names (`exclude: [sase-android]`)
  would be terser since the org is known, but `owner/name` matches every other repo reference in the config, reuses the
  existing `RepoName` parser/tests, and is greppable. The owner-must-match-org check (C2) catches the redundant-owner
  footgun without changing the format.
- **Error (don't silently ignore) on misplaced/foreign keys.** Silent no-ops are the worst outcome for a "why isn't my
  repo excluded?" debugging session.
- **Localized case-insensitivity.** Only the exclude comparison becomes case-insensitive; `RepoName`'s global
  equality/ordering stays exact so dedup/sort and `--repo` selection are unaffected.

## Out of scope

- No new CLI flags; exclusion stays config-driven (consistent with the existing `org:`/`repo:` model).
- No change to `RepoName` equality/ordering semantics.
- No edits to historical design docs or prior plan artifacts (`sdd/…`, `sase_plan_*.md`); their "Phase 3 / reserved"
  language is an accurate record of when it was written.
- No repo-existence validation for `exclude` entries (cannot be checked offline; a non-existent but well-formed
  `owner/name` simply matches nothing).
