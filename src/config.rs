//! Configuration file parsing for per-scope remotes and sibling stores.
//!
//! When a config file exists (`~/.config/memory-mcp/config.toml` or the path
//! in `MEMORY_MCP_CONFIG`), it defines scope-to-repo mappings that route
//! specific scopes to dedicated git repositories with their own remotes. The
//! same file can name trusted sibling Pali endpoints for federated reads.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use tracing::info;

use crate::error::MemoryError;
use crate::fs_util::expand_tilde;
use crate::types::ScopePath;

/// A single scope-to-repo mapping from the config file.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct RemoteMapping {
    /// Scope prefix that this mapping captures (e.g. `"work"` or `"org/team"`).
    pub scope: String,
    /// Git remote URL for this scope's repo.
    pub url: String,
    /// Local path for the git repo. Supports `~` expansion.
    /// Defaults to `~/.memory-mcp-{scope}` if omitted, with `/` in the scope
    /// encoded as `%2F` so distinct scopes always default to distinct paths.
    pub path: Option<String>,
    /// Branch name for push/pull. Defaults to the server-wide branch if omitted.
    pub branch: Option<String>,
}

/// Top-level config file structure.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Config {
    /// Per-scope remote mappings.
    #[serde(default)]
    pub remotes: Vec<RemoteMapping>,
    /// Stable provenance label for this Pali instance. Defaults to `local`.
    #[serde(default = "default_store_id")]
    pub store_id: String,
    /// Trusted sibling MCP endpoints keyed by their provenance label.
    ///
    /// This is topology only. Credentials are supplied by each caller and
    /// must never be stored here.
    #[serde(default)]
    pub siblings: BTreeMap<String, String>,
    /// Whole-request deadline for a straddled sibling read.
    #[serde(default = "default_straddle_timeout_ms")]
    pub straddle_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            remotes: Vec::new(),
            store_id: default_store_id(),
            siblings: BTreeMap::new(),
            straddle_timeout_ms: default_straddle_timeout_ms(),
        }
    }
}

fn default_store_id() -> String {
    "local".to_owned()
}

const fn default_straddle_timeout_ms() -> u64 {
    2_000
}

impl Config {
    /// Load config from the given path, returning `Config::default()` if the
    /// file does not exist.
    pub fn load(path: &Path) -> Result<Self, MemoryError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|e| {
            MemoryError::Internal(format!(
                "failed to read config file {}: {}",
                path.display(),
                e
            ))
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| {
            MemoryError::Internal(format!(
                "failed to parse config file {}: {}",
                path.display(),
                e
            ))
        })?;
        for mapping in &config.remotes {
            ScopePath::new(&mapping.scope).map_err(|_| MemoryError::InvalidInput {
                reason: format!(
                    "invalid scope '{}' in config file {}",
                    mapping.scope,
                    path.display()
                ),
            })?;
            // Every mapped branch is validated, not just the server-wide
            // default (#293 review, round 3): an invalid override must be
            // rejected here, not discovered at the first push/pull.
            if let Some(branch) = &mapping.branch {
                crate::types::validate_branch_name(branch).map_err(|_| {
                    MemoryError::InvalidInput {
                        reason: format!(
                            "invalid branch '{}' for scope '{}' in config file {}",
                            branch,
                            mapping.scope,
                            path.display()
                        ),
                    }
                })?;
            }
        }
        config.validate_siblings()?;
        info!(
            path = %path.display(),
            remotes = config.remotes.len(),
            siblings = config.siblings.len(),
            "loaded config"
        );
        Ok(config)
    }

    /// Validate sibling topology independently of deserialization.
    ///
    /// The MCP server calls this again when library users supply a `Config`
    /// directly, so an unvalidated struct can never bypass TLS or embedded-
    /// credential constraints.
    pub(crate) fn validate_siblings(&self) -> Result<(), MemoryError> {
        validate_store_id(&self.store_id, "store_id")?;
        if self.straddle_timeout_ms == 0 {
            return Err(MemoryError::InvalidInput {
                reason: "straddle_timeout_ms must be greater than zero".into(),
            });
        }
        for (store_id, endpoint) in &self.siblings {
            validate_store_id(store_id, "sibling store id")?;
            if store_id == &self.store_id {
                return Err(MemoryError::InvalidInput {
                    reason: format!("sibling store id '{store_id}' collides with store_id"),
                });
            }
            let url = reqwest::Url::parse(endpoint).map_err(|_| MemoryError::InvalidInput {
                reason: format!("invalid sibling endpoint for store '{store_id}'"),
            })?;
            let loopback_http = url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
            if url.scheme() != "https" && !loopback_http {
                return Err(MemoryError::InvalidInput {
                    reason: format!("sibling endpoint for store '{store_id}' must use https (http is allowed only for loopback tests)"),
                });
            }
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "sibling endpoint for store '{store_id}' must not contain credentials, query parameters, or fragments"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Resolve the config file path from the environment or default location.
    ///
    /// Resolution order:
    /// 1. `MEMORY_MCP_CONFIG` environment variable
    /// 2. `~/.config/memory-mcp/config.toml`
    pub fn resolve_path() -> Result<PathBuf, MemoryError> {
        if let Ok(env_path) = std::env::var("MEMORY_MCP_CONFIG") {
            return Ok(PathBuf::from(env_path));
        }
        let config_dir = dirs::config_dir()
            .ok_or_else(|| MemoryError::Internal("could not determine config directory".into()))?;
        Ok(config_dir.join("memory-mcp").join("config.toml"))
    }
}

fn validate_store_id(value: &str, field: &str) -> Result<(), MemoryError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(MemoryError::InvalidInput {
            reason: format!(
                "invalid {field} '{value}'; use 1-64 ASCII letters, digits, '-' or '_'"
            ),
        })
    }
}

impl RemoteMapping {
    /// Resolve the local repo path, expanding `~` and applying defaults.
    pub fn resolved_path(&self) -> Result<PathBuf, MemoryError> {
        match &self.path {
            Some(p) => expand_tilde(p),
            None => {
                let home = dirs::home_dir().ok_or_else(|| {
                    MemoryError::Internal("could not determine home directory".into())
                })?;
                // Encode `/` as `%2F`. Scope components cannot contain `%`,
                // so this is injective: mapping `/` to `-` made the scopes
                // `org/team` and `org-team` collide on the same default path
                // — and therefore the same physical repo (#293 review,
                // round 4).
                let dir_name = format!(".memory-mcp-{}", self.scope.replace('/', "%2F"));
                Ok(home.join(dir_name))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[[remotes]]
scope = "work"
url = "git@github.com:org/repo.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.remotes.len(), 1);
        assert_eq!(config.remotes[0].scope, "work");
        assert_eq!(config.remotes[0].url, "git@github.com:org/repo.git");
        assert!(config.remotes[0].path.is_none());
        assert!(config.remotes[0].branch.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[[remotes]]
scope = "work"
url = "git@github.com:org/repo.git"
path = "~/.memory-mcp-work"
branch = "main"

[[remotes]]
scope = "org/team"
url = "git@github.com:org/team-memories.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.remotes.len(), 2);
        assert_eq!(
            config.remotes[0].path.as_deref(),
            Some("~/.memory-mcp-work")
        );
        assert_eq!(config.remotes[0].branch.as_deref(), Some("main"));
        assert_eq!(config.remotes[1].scope, "org/team");
    }

    #[test]
    fn empty_config_has_no_remotes() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.remotes.is_empty());
        assert_eq!(config.store_id, "local");
        assert!(config.siblings.is_empty());
        assert_eq!(config.straddle_timeout_ms, 2_000);
    }

    #[test]
    fn parse_federation_topology_without_credentials() {
        let config: Config = toml::from_str(
            r#"
store_id = "personal"
straddle_timeout_ms = 750

[siblings]
fcc = "https://friends.example/mcp"
lcc = "https://lacuna.example/mcp"
"#,
        )
        .unwrap();

        assert_eq!(config.store_id, "personal");
        assert_eq!(config.straddle_timeout_ms, 750);
        assert_eq!(config.siblings["fcc"], "https://friends.example/mcp");
    }

    #[test]
    fn load_rejects_local_store_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
store_id = "personal"
[siblings]
personal = "https://personal.example/mcp"
"#,
        )
        .unwrap();

        let error = Config::load(&path).expect_err("store collision must fail startup");
        assert!(error.to_string().contains("collides with store_id"));
    }

    #[test]
    fn load_rejects_plaintext_non_loopback_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[siblings]
fcc = "http://friends.example/mcp"
"#,
        )
        .unwrap();

        let error = Config::load(&path).expect_err("plaintext remote sibling must fail startup");
        assert!(error.to_string().contains("must use https"));
    }

    #[test]
    fn load_rejects_credentials_in_sibling_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[siblings]
fcc = "https://token@friends.example/mcp"
"#,
        )
        .unwrap();

        let error = Config::load(&path).expect_err("endpoint credentials must fail startup");
        assert!(error.to_string().contains("must not contain credentials"));
    }

    #[test]
    fn default_path_uses_scope() {
        let mapping = RemoteMapping {
            scope: "work".to_string(),
            url: "https://example.com/repo.git".to_string(),
            path: None,
            branch: None,
        };
        let resolved = mapping.resolved_path().unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolved, home.join(".memory-mcp-work"));
    }

    #[test]
    fn default_path_encodes_slashes() {
        let mapping = RemoteMapping {
            scope: "org/team".to_string(),
            url: "https://example.com/repo.git".to_string(),
            path: None,
            branch: None,
        };
        let resolved = mapping.resolved_path().unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolved, home.join(".memory-mcp-org%2Fteam"));
    }

    /// Default path encoding must be injective (#293 review, round 4):
    /// mapping `/` to `-` sent the scopes `org/team` and `org-team` to the
    /// same default path, so two nominally isolated scopes shared one
    /// physical repo under separate mutexes.
    #[test]
    fn default_paths_distinguish_slash_from_dash_scopes() {
        let path_for = |scope: &str| {
            RemoteMapping {
                scope: scope.to_string(),
                url: "https://example.com/repo.git".to_string(),
                path: None,
                branch: None,
            }
            .resolved_path()
            .unwrap()
        };
        assert_ne!(path_for("org/team"), path_for("org-team"));
    }

    #[test]
    fn expand_tilde_home() {
        let result = expand_tilde("~/foo/bar").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("foo/bar"));
    }

    #[test]
    fn expand_tilde_absolute_passthrough() {
        let result = expand_tilde("/tmp/repo").unwrap();
        assert_eq!(result, PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn expand_tilde_user_rejected() {
        let result = expand_tilde("~otheruser/path");
        assert!(result.is_err());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let config = Config::load(Path::new("/nonexistent/path/config.toml")).unwrap();
        assert!(config.remotes.is_empty());
    }

    /// Every mapped branch is validated at config load (#293 review,
    /// round 3) — an invalid override must be rejected here, not discovered
    /// at the first push/pull against that repo.
    #[test]
    fn load_rejects_invalid_mapped_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[remotes]]
scope = "ok"
url = "https://example.com/ok.git"
branch = "main"

[[remotes]]
scope = "work"
url = "https://example.com/repo.git"
branch = "bad..branch"
"#,
        )
        .unwrap();
        let err = Config::load(&path).expect_err("an invalid mapped branch must fail the load");
        let msg = err.to_string();
        assert!(
            msg.contains("bad..branch") && msg.contains("work"),
            "the error must name the offending branch and scope: {msg}"
        );
    }
}
