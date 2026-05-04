//! User-level config — GitHub/GitLab tokens and a few related knobs that
//! the dashboard's refresh button needs to hit private repos.
//!
//! Lives at `~/.config/mercator/config.toml` (mode 0600 on unix). Loaded
//! once at `mercator serve` startup and stashed behind an
//! `Arc<Mutex<Config>>` in `AppState`. Closes the dead-UI half of #2 —
//! the CLI has accepted `--github-token` / `--gitlab-token` since
//! before the issue was filed; this is the missing server-side store
//! the dashboard's refresh path needs.
//!
//! Token redaction lives in [`Config::redacted`]: the
//! `GET /api/settings` handler uses it to expose user names + a
//! `*-token-set` boolean without ever shipping the raw secret to the
//! browser.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-provider config. Both fields are optional — a present `user`
/// without a `token` is a "fetch public repos only" setup, valid (and
/// rate-limited at 60/hr for GitHub, per its own rules).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl ProviderConfig {
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref().filter(|s| !s.is_empty())
    }
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub github: ProviderConfig,
    #[serde(default)]
    pub gitlab: ProviderConfig,
}

/// What `GET /api/settings` returns — never includes the raw token.
/// Stays a separate type so an accidental serialization of `Config`
/// can't leak the secret by mistake.
#[derive(Debug, Serialize)]
pub struct RedactedConfig {
    pub github_user: Option<String>,
    pub github_token_set: bool,
    pub gitlab_user: Option<String>,
    pub gitlab_token_set: bool,
}

impl Config {
    pub fn redacted(&self) -> RedactedConfig {
        RedactedConfig {
            github_user: self.github.user().map(str::to_string),
            github_token_set: self.github.token().is_some(),
            gitlab_user: self.gitlab.user().map(str::to_string),
            gitlab_token_set: self.gitlab.token().is_some(),
        }
    }
}

/// `~/.config/mercator/config.toml`. Falls back to `./config.toml` if
/// the home directory can't be resolved (rare; keeps tests deterministic
/// when run in environments without `HOME` set).
pub fn config_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".config").join("mercator").join("config.toml")
}

/// Read the config from disk. A missing or unreadable file yields the
/// `Default` config (empty providers). Parse errors propagate so the
/// user sees the `eprintln!` warning at startup.
pub fn load_from(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    toml::from_str(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))
}

/// Convenience wrapper around `load_from(config_path())`.
pub fn load() -> Result<Config, String> {
    load_from(&config_path())
}

/// Write the config atomically. On unix the file mode is set to 0600
/// (owner-only read/write) since it carries tokens. On other platforms
/// the file is written with the OS default permissions.
pub fn save_to(path: &Path, cfg: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let serialized = toml::to_string_pretty(cfg).map_err(|e| format!("serialize config: {}", e))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &serialized).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    set_owner_only_perms(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {}", tmp.display(), e))?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_perms(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {}", path.display(), e))?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn set_owner_only_perms(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_from(&dir.path().join("missing.toml")).unwrap();
        assert!(cfg.github.user().is_none());
        assert!(cfg.gitlab.token().is_none());
    }

    #[test]
    fn round_trips_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.github.user = Some("alice".into());
        cfg.github.token = Some("ghp_secret".into());
        cfg.gitlab.user = Some("bob".into());
        save_to(&path, &cfg).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.github.user(), Some("alice"));
        assert_eq!(loaded.github.token(), Some("ghp_secret"));
        assert_eq!(loaded.gitlab.user(), Some("bob"));
        assert!(loaded.gitlab.token().is_none());
    }

    #[test]
    fn empty_strings_treated_as_unset() {
        let cfg = Config {
            github: ProviderConfig {
                user: Some("".into()),
                token: Some("".into()),
            },
            ..Default::default()
        };
        // Empty user/token are accepted by the file format but the helper
        // accessors filter them so call sites can use `if let Some(...)`
        // without branching for empty.
        assert!(cfg.github.user().is_none());
        assert!(cfg.github.token().is_none());
    }

    #[test]
    fn redacted_never_includes_token() {
        let cfg = Config {
            github: ProviderConfig {
                user: Some("alice".into()),
                token: Some("ghp_secret".into()),
            },
            gitlab: ProviderConfig {
                user: None,
                token: None,
            },
        };
        let r = cfg.redacted();
        assert_eq!(r.github_user.as_deref(), Some("alice"));
        assert!(r.github_token_set);
        assert!(r.gitlab_user.is_none());
        assert!(!r.gitlab_token_set);

        // Sanity-check via JSON: the serialized payload mustn't leak.
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("ghp_secret"),
            "redacted leaked token: {json}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_owner_only_perms_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            github: ProviderConfig {
                user: Some("alice".into()),
                token: Some("secret".into()),
            },
            ..Default::default()
        };
        save_to(&path, &cfg).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:#o}");
    }
}
