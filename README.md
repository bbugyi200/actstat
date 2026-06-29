# actstat

A fast, beautiful Rust CLI that reports the status of the most recent completed
GitHub Actions workflow runs across a configured set of repositories.

> **Status:** under construction. This README currently documents configuration
> (Phase 2); a full guide (install, usage, output examples, exit codes, cronjob
> recipe) lands with the final documentation pass.

## Configuration

`actstat` reads a small YAML config that lists the **projects** to inspect. Each
entry is either an entire **org** (expanded to all of its repositories) or a
single **repo**:

```yaml
# ~/.config/actstat/config.yml
projects:
  - org: sase-org # all repositories in the sase-org organization
  - org: bobs-org # all repositories in the bobs-org organization
  - repo: bbugyi200/dotfiles # a single repository
  - repo: bbugyi200/actstat # this repository
```

No secrets ever go in the config — `actstat` discovers its GitHub token from the
environment (`ACTSTAT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`) or from
`gh auth token`.

### Where the config is found

The config path is resolved in this order; the first hit wins:

1. `--config PATH`
2. `ACTSTAT_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/actstat/config.yml`
4. `~/.config/actstat/config.yml`

`--config` and `ACTSTAT_CONFIG` are explicit overrides and are used verbatim;
the two well-known locations are tried in order and the first existing file is
used. If none is found, `actstat` exits with an actionable message and a minimal
example config.

### Managing the config with chezmoi

The real machine config is managed by chezmoi. Its source lives at
`home/dot_config/actstat/config.yml` in the chezmoi source repo (the `home/`
source root is set by `.chezmoiroot`), so it materializes at
`~/.config/actstat/config.yml`:

```sh
chezmoi apply ~/.config/actstat/config.yml
```

### Overriding the config (e.g. for tests or ad-hoc runs)

Point `actstat` at a different file without touching the managed config by
setting `ACTSTAT_CONFIG` or passing `--config`:

```sh
ACTSTAT_CONFIG=/tmp/actstat-test.yml actstat list
actstat list --config ./fixtures/config.yml
```
