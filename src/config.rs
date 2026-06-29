//! Configuration model, discovery, parsing, validation, and source resolution.
//!
//! The config uses the user's own vocabulary (`projects`) and a small,
//! forward-compatible shape. Each project entry is either an **org** (expanded
//! to that organization's repositories, with optional filters) or a single
//! **repo**:
//!
//! ```yaml
//! projects:
//!   - org: sase-org
//!   - org: bobs-org
//!   - repo: bbugyi200/dotfiles
//!   - repo: bbugyi200/actstat
//! ```
//!
//! This module owns four concerns:
//!
//! 1. **Discovery** — locating the config file via the documented precedence
//!    (`--config` → `ACTSTAT_CONFIG` → `$XDG_CONFIG_HOME` → `~/.config`).
//! 2. **Parsing** — deserializing the YAML with `serde` (via `serde_norway`).
//! 3. **Validation** — turning the loose YAML into a checked [`Config`] with
//!    clear, actionable errors.
//! 4. **Resolution** — the pure merge/dedup/sort that combines explicitly
//!    configured repos with org-expanded repos into a single de-duplicated,
//!    alphabetically-stable list of `owner/name`.
//!
//! No secrets ever live in the config; tokens come only from the environment or
//! `gh` (see [`crate::github`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Relative location of the config file under any config root.
const REL_PATH: &str = "actstat/config.yml";

/// Errors that can occur while locating, reading, or parsing the config file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No config file was found via any discovery source.
    #[error(
        "no actstat config found (looked in: {0}); \
         create one or pass --config PATH / set ACTSTAT_CONFIG"
    )]
    NotFound(String),

    /// The config file could not be read.
    #[error("failed to read config at {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config file was found but could not be parsed/validated.
    #[error("invalid config at {path}: {message}")]
    Invalid {
        /// Path of the offending config file.
        path: String,
        /// Human-readable explanation of what was wrong.
        message: String,
    },
}

/// A validated actstat configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The configured project sources, in declaration order.
    pub projects: Vec<ProjectSource>,
}

/// One configured source of repositories: an entire org or a single repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSource {
    /// Every repository in an organization, with supported expansion filters.
    Org(OrgSource),
    /// Exactly one repository.
    Repo(RepoName),
}

/// An organization source plus its expansion filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgSource {
    /// Bare organization login (e.g. `sase-org`).
    pub org: String,
    /// Include archived repositories during expansion. Defaults to `false`.
    pub include_archived: bool,
    /// Include forks during expansion. Defaults to `false`.
    pub include_forks: bool,
    /// Repositories to drop from this org's expansion.
    pub exclude: Vec<RepoName>,
}

/// A `owner/name` repository identifier.
///
/// Ordering is by the rendered `owner/name` string so that dedup/sort matches
/// the documented "alphabetically stable list of `owner/name`" exactly (the
/// `/` separator participates in the comparison, unlike a naive owner-then-name
/// tuple ordering).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoName {
    /// The repository owner (org or user login).
    pub owner: String,
    /// The repository name.
    pub name: String,
}

impl RepoName {
    /// Parse and validate an `owner/name` string. Surrounding whitespace is
    /// tolerated; internal whitespace, a missing/extra `/`, or an empty side
    /// is rejected with an actionable message.
    pub fn parse(s: &str) -> Result<RepoName, String> {
        let trimmed = s.trim();
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() != 2 {
            return Err(format!(
                "repo `{trimmed}` must be in `owner/name` form"
            ));
        }
        let owner = parts[0].trim();
        let name = parts[1].trim();
        if owner.is_empty() || name.is_empty() {
            return Err(format!(
                "repo `{trimmed}` must be in `owner/name` form \
                 (both owner and name are required)"
            ));
        }
        if owner.contains(char::is_whitespace)
            || name.contains(char::is_whitespace)
        {
            return Err(format!(
                "repo `{trimmed}` must not contain whitespace"
            ));
        }
        Ok(RepoName {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// The canonical `owner/name` rendering.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl std::fmt::Display for RepoName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl Ord for RepoName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.full_name().cmp(&other.full_name())
    }
}

impl PartialOrd for RepoName {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Config {
    /// The single-repo entries from the config (org entries excluded).
    pub fn explicit_repos(&self) -> impl Iterator<Item = &RepoName> {
        self.projects.iter().filter_map(|p| match p {
            ProjectSource::Repo(r) => Some(r),
            ProjectSource::Org(_) => None,
        })
    }

    /// The org entries from the config.
    pub fn orgs(&self) -> impl Iterator<Item = &OrgSource> {
        self.projects.iter().filter_map(|p| match p {
            ProjectSource::Org(o) => Some(o),
            ProjectSource::Repo(_) => None,
        })
    }
}

/// Merge explicitly-configured repos with org-expanded repos into the final
/// de-duplicated, alphabetically-stable list of `owner/name`.
///
/// A repo named both explicitly and via its org appears exactly once.
pub fn resolve_repos(
    explicit: impl IntoIterator<Item = RepoName>,
    expanded: impl IntoIterator<Item = RepoName>,
) -> Vec<RepoName> {
    let mut set: BTreeSet<RepoName> = BTreeSet::new();
    set.extend(explicit);
    set.extend(expanded);
    set.into_iter().collect()
}

/// A minimal, valid example config, used in the "no config found" guidance and
/// as documentation. Kept in sync with the real schema by a unit test.
pub fn example_config() -> &'static str {
    "# ~/.config/actstat/config.yml\n\
     projects:\n\
     \x20 - org: sase-org\n\
     \x20 - repo: bbugyi200/actstat\n"
}

/// Discover the config path, read it, and parse + validate it.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let path = discover_path(explicit)?;
    load_file(&path)
}

/// Read and parse + validate the config at an exact path (no discovery).
pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    let text =
        std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
    parse_str(&text, &path.display().to_string())
}

/// Parse + validate config text. `source_path` only labels error messages, so
/// this is fully unit-testable without touching the filesystem.
pub fn parse_str(text: &str, source_path: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = serde_norway::from_str(text)
        .map_err(|e| invalid(source_path, format!("YAML parse error: {e}")))?;
    validate(raw, source_path)
}

/// Resolve the config path using the documented precedence:
///
/// 1. `--config PATH` (explicit; used verbatim, existence checked at read time)
/// 2. `ACTSTAT_CONFIG` (explicit override; used verbatim)
/// 3. `$XDG_CONFIG_HOME/actstat/config.yml` (used if the file exists)
/// 4. `~/.config/actstat/config.yml` (used if the file exists)
///
/// The two explicit sources do not fall through (they reflect user intent);
/// the two well-known locations are tried in order and the first existing file
/// wins. If nothing is found, [`ConfigError::NotFound`] lists what was searched.
pub fn discover_path(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
    discover_path_in(explicit, &DiscoveryEnv::from_process())
}

/// The environment inputs that drive discovery, factored out so tests can
/// exercise every branch without mutating process-global environment state.
struct DiscoveryEnv {
    actstat_config: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl DiscoveryEnv {
    fn from_process() -> Self {
        DiscoveryEnv {
            actstat_config: std::env::var_os("ACTSTAT_CONFIG")
                .map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// Pure discovery against an injected [`DiscoveryEnv`].
fn discover_path_in(
    explicit: Option<&Path>,
    env: &DiscoveryEnv,
) -> Result<PathBuf, ConfigError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = &env.actstat_config {
        return Ok(p.clone());
    }

    let mut searched = Vec::new();
    for root in [env.xdg_config_home.as_deref(), home_config(env).as_deref()] {
        let Some(root) = root else { continue };
        let candidate = root.join(REL_PATH);
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(candidate.display().to_string());
    }
    Err(ConfigError::NotFound(searched.join(", ")))
}

/// `~/.config` as a config root, if `HOME` is known.
fn home_config(env: &DiscoveryEnv) -> Option<PathBuf> {
    env.home.as_ref().map(|h| h.join(".config"))
}

/// Loose, serde-shaped mirror of the config file before validation.
#[derive(Debug, Deserialize)]
struct RawConfig {
    /// Optional so a missing `projects:` key is distinguishable and reported
    /// with a tailored message rather than a generic serde error.
    #[serde(default)]
    projects: Option<Vec<RawProject>>,
}

/// A single raw project entry. Unknown fields are intentionally **allowed** so
/// future config fields can be added without breaking older binaries.
#[derive(Debug, Deserialize)]
struct RawProject {
    #[serde(default)]
    org: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    include_archived: Option<bool>,
    #[serde(default)]
    include_forks: Option<bool>,
    #[serde(default)]
    exclude: Option<Vec<String>>,
}

/// Turn a parsed [`RawConfig`] into a checked [`Config`], or an actionable
/// [`ConfigError::Invalid`].
fn validate(raw: RawConfig, path: &str) -> Result<Config, ConfigError> {
    let raw_projects = raw.projects.unwrap_or_default();
    if raw_projects.is_empty() {
        return Err(invalid(
            path,
            "config has no `projects`: add at least one `org:` or `repo:` entry",
        ));
    }

    let mut projects = Vec::with_capacity(raw_projects.len());
    for (i, rp) in raw_projects.into_iter().enumerate() {
        let n = i + 1;
        let source = match (rp.org, rp.repo) {
            (Some(_), Some(_)) => {
                return Err(invalid(
                    path,
                    format!(
                        "project #{n} sets both `org` and `repo`; \
                         each entry must set exactly one"
                    ),
                ));
            }
            (None, None) => {
                return Err(invalid(
                    path,
                    format!(
                        "project #{n} sets neither `org` nor `repo`; \
                         each entry must set exactly one"
                    ),
                ));
            }
            (Some(org), None) => ProjectSource::Org(validate_org(
                org,
                &rp.exclude,
                rp.include_archived,
                rp.include_forks,
                n,
                path,
            )?),
            (None, Some(repo)) => {
                validate_repo_entry_fields(
                    &rp.exclude,
                    rp.include_archived,
                    rp.include_forks,
                    n,
                    path,
                )?;
                let name = RepoName::parse(&repo)
                    .map_err(|e| invalid(path, format!("project #{n}: {e}")))?;
                ProjectSource::Repo(name)
            }
        };
        projects.push(source);
    }

    Ok(Config { projects })
}

/// Validate a single repo entry's org-only fields.
fn validate_repo_entry_fields(
    exclude: &Option<Vec<String>>,
    include_archived: Option<bool>,
    include_forks: Option<bool>,
    n: usize,
    path: &str,
) -> Result<(), ConfigError> {
    if exclude.is_some() {
        return Err(org_only_field_error(path, n, "exclude"));
    }
    if include_archived.is_some() {
        return Err(org_only_field_error(path, n, "include_archived"));
    }
    if include_forks.is_some() {
        return Err(org_only_field_error(path, n, "include_forks"));
    }
    Ok(())
}

/// Build the shared error for a known org-only key used on a repo entry.
fn org_only_field_error(path: &str, n: usize, field: &str) -> ConfigError {
    invalid(
        path,
        format!(
            "project #{n}: `{field}` only applies to `org:` entries, not `repo:`"
        ),
    )
}

/// Validate a single org entry and its filters.
fn validate_org(
    org: String,
    exclude: &Option<Vec<String>>,
    include_archived: Option<bool>,
    include_forks: Option<bool>,
    n: usize,
    path: &str,
) -> Result<OrgSource, ConfigError> {
    let org = org.trim();
    if org.is_empty() {
        return Err(invalid(
            path,
            format!("project #{n}: `org` must not be empty"),
        ));
    }
    if org.contains('/') {
        return Err(invalid(
            path,
            format!(
                "project #{n}: `org` must be a bare organization name, not `{org}`"
            ),
        ));
    }

    let mut excluded = Vec::new();
    for entry in exclude.iter().flatten() {
        let name = RepoName::parse(entry).map_err(|e| {
            invalid(path, format!("project #{n} `exclude`: {e}"))
        })?;
        if !name.owner.eq_ignore_ascii_case(org) {
            return Err(invalid(
                path,
                format!(
                    "project #{n} `exclude`: `{name}` is not in org `{org}`"
                ),
            ));
        }
        excluded.push(name);
    }

    Ok(OrgSource {
        org: org.to_string(),
        include_archived: include_archived.unwrap_or(false),
        include_forks: include_forks.unwrap_or(false),
        exclude: excluded,
    })
}

/// Build a [`ConfigError::Invalid`] with the offending path and a message.
fn invalid(path: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn repo(owner: &str, name: &str) -> RepoName {
        RepoName {
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }

    // --- Parsing & validation -------------------------------------------

    #[test]
    fn parses_the_documented_four_source_config() {
        let text = "\
projects:
  - org: sase-org
  - org: bobs-org
  - repo: bbugyi200/dotfiles
  - repo: bbugyi200/actstat
";
        let cfg = parse_str(text, "<test>").expect("documented config parses");
        assert_eq!(cfg.projects.len(), 4);

        let orgs: Vec<&str> = cfg.orgs().map(|o| o.org.as_str()).collect();
        assert_eq!(orgs, vec!["sase-org", "bobs-org"]);

        let repos: Vec<String> =
            cfg.explicit_repos().map(RepoName::full_name).collect();
        assert_eq!(
            repos,
            vec![
                "bbugyi200/dotfiles".to_string(),
                "bbugyi200/actstat".to_string()
            ]
        );
    }

    #[test]
    fn repo_entry_parses_into_owner_and_name() {
        let cfg =
            parse_str("projects:\n  - repo: octocat/Hello-World\n", "<t>")
                .unwrap();
        assert_eq!(
            cfg.projects,
            vec![ProjectSource::Repo(repo("octocat", "Hello-World"))]
        );
    }

    #[test]
    fn org_entry_defaults_filters() {
        let cfg = parse_str("projects:\n  - org: sase-org\n", "<t>").unwrap();
        let org = cfg.orgs().next().unwrap();
        assert_eq!(org.org, "sase-org");
        assert!(!org.include_archived);
        assert!(!org.include_forks);
        assert!(org.exclude.is_empty());
    }

    #[test]
    fn org_entry_parses_org_filters() {
        let text = "\
projects:
  - org: sase-org
    include_archived: true
    include_forks: true
    exclude: [sase-org/scratch, sase-org/old]
";
        let cfg = parse_str(text, "<t>").expect("org filters parse");
        let org = cfg.orgs().next().unwrap();
        assert!(org.include_archived);
        assert!(org.include_forks);
        assert_eq!(
            org.exclude,
            vec![repo("sase-org", "scratch"), repo("sase-org", "old")]
        );
    }

    #[test]
    fn unknown_future_fields_are_ignored_not_rejected() {
        // Forward-compatibility: a field this binary doesn't know about must
        // not break parsing.
        let cfg = parse_str(
            "projects:\n  - org: sase-org\n    some_future_filter: 7\n",
            "<t>",
        )
        .expect("unknown fields tolerated");
        assert_eq!(cfg.orgs().count(), 1);
    }

    #[test]
    fn missing_projects_key_is_an_error() {
        let err = parse_str("defaults: {}\n", "cfg.yml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no `projects`"), "got: {msg}");
    }

    #[test]
    fn empty_projects_list_is_an_error() {
        let err = parse_str("projects: []\n", "cfg.yml").unwrap_err();
        assert!(err.to_string().contains("no `projects`"));
    }

    #[test]
    fn entry_with_neither_org_nor_repo_is_an_error() {
        let err =
            parse_str("projects:\n  - name: nope\n", "cfg.yml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("neither"), "got: {msg}");
    }

    #[test]
    fn entry_with_both_org_and_repo_is_an_error() {
        let err =
            parse_str("projects:\n  - org: x\n    repo: a/b\n", "cfg.yml")
                .unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn malformed_repo_without_slash_is_an_error() {
        let err = parse_str("projects:\n  - repo: justname\n", "cfg.yml")
            .unwrap_err();
        assert!(err.to_string().contains("owner/name"));
    }

    #[test]
    fn malformed_repo_with_extra_slash_is_an_error() {
        let err =
            parse_str("projects:\n  - repo: a/b/c\n", "cfg.yml").unwrap_err();
        assert!(err.to_string().contains("owner/name"));
    }

    #[test]
    fn malformed_repo_with_empty_side_is_an_error() {
        let err =
            parse_str("projects:\n  - repo: owner/\n", "cfg.yml").unwrap_err();
        assert!(err.to_string().contains("owner/name"));
    }

    #[test]
    fn repo_entry_rejects_org_only_fields() {
        for (field, line) in [
            ("exclude", "    exclude: [o/skip]\n"),
            ("include_archived", "    include_archived: true\n"),
            ("include_forks", "    include_forks: true\n"),
        ] {
            let text = format!("projects:\n  - repo: o/r\n{line}");
            let err = parse_str(&text, "cfg.yml").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(field), "got: {msg}");
            assert!(msg.contains("org:"), "got: {msg}");
            assert!(msg.contains("repo:"), "got: {msg}");
        }
    }

    #[test]
    fn org_containing_a_slash_is_an_error() {
        let err =
            parse_str("projects:\n  - org: a/b\n", "cfg.yml").unwrap_err();
        assert!(err.to_string().contains("bare organization name"));
    }

    #[test]
    fn malformed_exclude_entry_is_an_error() {
        let err = parse_str(
            "projects:\n  - org: sase-org\n    exclude: [not-a-repo]\n",
            "cfg.yml",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exclude"), "got: {msg}");
    }

    #[test]
    fn exclude_entry_with_foreign_owner_is_an_error() {
        let err = parse_str(
            "projects:\n  - org: sase-org\n    exclude: [other-org/scratch]\n",
            "cfg.yml",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exclude"), "got: {msg}");
        assert!(msg.contains("other-org/scratch"), "got: {msg}");
        assert!(msg.contains("sase-org"), "got: {msg}");
    }

    #[test]
    fn exclude_owner_matching_org_with_different_case_is_allowed() {
        let cfg = parse_str(
            "projects:\n  - org: Sase-Org\n    exclude: [sase-org/scratch]\n",
            "cfg.yml",
        )
        .expect("GitHub org names are case-insensitive");
        let org = cfg.orgs().next().unwrap();
        assert_eq!(org.exclude, vec![repo("sase-org", "scratch")]);
    }

    #[test]
    fn yaml_syntax_error_is_reported() {
        let err = parse_str("projects:\n  - org: [unterminated\n", "cfg.yml")
            .unwrap_err();
        assert!(err.to_string().contains("YAML parse error"));
    }

    #[test]
    fn error_messages_carry_the_source_path() {
        let err =
            parse_str("projects: []\n", "/some/where/config.yml").unwrap_err();
        assert!(err.to_string().contains("/some/where/config.yml"));
    }

    // --- RepoName -------------------------------------------------------

    #[test]
    fn repo_name_trims_surrounding_whitespace() {
        assert_eq!(RepoName::parse("  a/b  ").unwrap(), repo("a", "b"));
    }

    #[test]
    fn repo_name_rejects_internal_whitespace() {
        assert!(RepoName::parse("a b/c").is_err());
    }

    #[test]
    fn repo_name_display_is_owner_slash_name() {
        assert_eq!(repo("octo", "cat").to_string(), "octo/cat");
    }

    // --- Source resolution: dedup + sort --------------------------------

    #[test]
    fn resolve_dedups_and_sorts_alphabetically() {
        let explicit = vec![repo("bbugyi200", "actstat"), repo("a", "z")];
        let expanded =
            vec![repo("sase-org", "tools"), repo("a", "a"), repo("a", "z")];
        let merged = resolve_repos(explicit, expanded);
        let names: Vec<String> =
            merged.iter().map(RepoName::full_name).collect();
        assert_eq!(
            names,
            vec![
                "a/a".to_string(),
                "a/z".to_string(),
                "bbugyi200/actstat".to_string(),
                "sase-org/tools".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_collapses_repo_named_both_explicitly_and_via_org() {
        // A repo listed explicitly and also surfaced by org expansion appears
        // exactly once.
        let explicit = vec![repo("sase-org", "tools")];
        let expanded =
            vec![repo("sase-org", "tools"), repo("sase-org", "other")];
        let merged = resolve_repos(explicit, expanded);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged.iter().map(RepoName::full_name).collect::<Vec<_>>(),
            vec!["sase-org/other".to_string(), "sase-org/tools".to_string()]
        );
    }

    #[test]
    fn ordering_uses_the_full_owner_slash_name_string() {
        // `-` (0x2d) sorts before `/` (0x2f), so "a-x/b" precedes "a/b" when
        // comparing the rendered names — the stable, documented behavior.
        let merged =
            resolve_repos(vec![repo("a", "b"), repo("a-x", "b")], vec![]);
        assert_eq!(
            merged.iter().map(RepoName::full_name).collect::<Vec<_>>(),
            vec!["a-x/b".to_string(), "a/b".to_string()]
        );
    }

    // --- The example config stays valid ---------------------------------

    #[test]
    fn example_config_parses_into_two_sources() {
        let cfg = parse_str(example_config(), "<example>")
            .expect("example config must stay valid");
        assert_eq!(cfg.orgs().count(), 1);
        assert_eq!(cfg.explicit_repos().count(), 1);
    }

    // --- Discovery ------------------------------------------------------

    fn write_config(dir: &Path) -> PathBuf {
        let path = dir.join(REL_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "projects:\n  - org: sase-org\n").unwrap();
        path
    }

    #[test]
    fn discovery_explicit_config_wins_outright() {
        let env = DiscoveryEnv {
            actstat_config: Some(PathBuf::from("/env/config.yml")),
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
        };
        let explicit = PathBuf::from("/explicit/config.yml");
        let found = discover_path_in(Some(&explicit), &env).unwrap();
        assert_eq!(found, explicit);
    }

    #[test]
    fn discovery_actstat_config_env_overrides_well_known_paths() {
        let env = DiscoveryEnv {
            actstat_config: Some(PathBuf::from("/env/config.yml")),
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
        };
        let found = discover_path_in(None, &env).unwrap();
        assert_eq!(found, PathBuf::from("/env/config.yml"));
    }

    #[test]
    fn discovery_uses_xdg_when_the_file_exists() {
        let xdg = TempDir::new().unwrap();
        let expected = write_config(xdg.path());
        let env = DiscoveryEnv {
            actstat_config: None,
            xdg_config_home: Some(xdg.path().to_path_buf()),
            home: Some(PathBuf::from("/home/u")),
        };
        let found = discover_path_in(None, &env).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn discovery_falls_back_to_home_when_xdg_has_no_file() {
        let xdg = TempDir::new().unwrap(); // empty: no config here
        let home = TempDir::new().unwrap();
        let expected = write_config(&home.path().join(".config"));
        let env = DiscoveryEnv {
            actstat_config: None,
            xdg_config_home: Some(xdg.path().to_path_buf()),
            home: Some(home.path().to_path_buf()),
        };
        let found = discover_path_in(None, &env).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn discovery_reports_not_found_listing_searched_paths() {
        let xdg = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let env = DiscoveryEnv {
            actstat_config: None,
            xdg_config_home: Some(xdg.path().to_path_buf()),
            home: Some(home.path().to_path_buf()),
        };
        let err = discover_path_in(None, &env).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no actstat config found"), "got: {msg}");
        assert!(msg.contains(REL_PATH), "searched paths listed: {msg}");
    }

    #[test]
    fn load_file_reads_parses_and_validates() {
        let dir = TempDir::new().unwrap();
        let path = write_config(dir.path());
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.orgs().next().unwrap().org, "sase-org");
    }

    #[test]
    fn load_file_missing_path_is_a_read_error() {
        let err =
            load_file(Path::new("/no/such/actstat/config.yml")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }
}
