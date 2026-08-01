//! Daemon configuration: struct defaults -> TOML file -> env overrides.
//!
//! [ollama] is intentionally absent here (CE-5).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::paths;

const ENV_BIND: &str = "MEMGARDEN_BIND";
const ENV_DB_PATH: &str = "MEMGARDEN_DB_PATH";
const ENV_LOG_LEVEL: &str = "MEMGARDEN_LOG_LEVEL";
const ENV_METRICS_INTERVAL: &str = "MEMGARDEN_METRICS_INTERVAL";
const ENV_CONFIG: &str = "MEMGARDEN_CONFIG";
const ENV_HOME: &str = "HOME";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub bind: String,
    pub db_path: PathBuf,
    pub log_level: String,
    pub metrics_snapshot_interval_secs: u64,
}

impl Config {
    /// Struct defaults: bind 127.0.0.1:9100, db_path = XDG default, log
    /// info, metrics snapshot every 60s.
    pub fn defaults() -> Result<Config> {
        Ok(Config {
            bind: "127.0.0.1:9100".to_string(),
            db_path: paths::default_db_path()?,
            log_level: "info".to_string(),
            metrics_snapshot_interval_secs: 60,
        })
    }

    /// Reads `$MEMGARDEN_CONFIG` (or the XDG config path if unset), the
    /// process environment, and merges them onto the struct defaults.
    pub fn load() -> Result<Config> {
        let defaults = Config::defaults()?;

        let config_path = match std::env::var(ENV_CONFIG) {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => paths::config_path()?,
        };
        let toml_str = match std::fs::read_to_string(&config_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::Io {
                    path: config_path,
                    source,
                });
            }
        };

        let mut env = HashMap::new();
        for key in [
            ENV_BIND,
            ENV_DB_PATH,
            ENV_LOG_LEVEL,
            ENV_METRICS_INTERVAL,
            ENV_HOME,
        ] {
            if let Ok(v) = std::env::var(key) {
                env.insert(key.to_string(), v);
            }
        }

        from_parts(defaults, toml_str.as_deref(), &env)
    }
}

/// Pure merge: `defaults` overridden by `toml_str` (if any) overridden by
/// `env` (looked up by the `MEMGARDEN_*` keys; `HOME` is consulted only for
/// `~` expansion of a db_path supplied via TOML or env). Kept pure and
/// side-effect-free so precedence and error paths are unit-testable without
/// touching the real filesystem or environment.
pub fn from_parts(
    defaults: Config,
    toml_str: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<Config> {
    let mut cfg = defaults;
    let home = env.get(ENV_HOME).map(String::as_str);

    if let Some(s) = toml_str {
        let parsed: TomlConfig =
            toml::from_str(s).map_err(|e| Error::Config(format!("malformed config TOML: {e}")))?;
        if let Some(bind) = parsed.server.and_then(|s| s.bind) {
            cfg.bind = bind;
        }
        if let Some(db_path) = parsed.storage.and_then(|s| s.db_path) {
            cfg.db_path = expand_tilde(&db_path, home);
        }
        if let Some(level) = parsed.log.and_then(|l| l.level) {
            cfg.log_level = level;
        }
        if let Some(secs) = parsed.metrics.and_then(|m| m.snapshot_interval_secs) {
            cfg.metrics_snapshot_interval_secs = secs;
        }
    }

    if let Some(bind) = env.get(ENV_BIND) {
        cfg.bind = bind.clone();
    }
    if let Some(db_path) = env.get(ENV_DB_PATH) {
        cfg.db_path = expand_tilde(db_path, home);
    }
    if let Some(level) = env.get(ENV_LOG_LEVEL) {
        cfg.log_level = level.clone();
    }
    if let Some(secs) = env.get(ENV_METRICS_INTERVAL) {
        cfg.metrics_snapshot_interval_secs = secs
            .parse()
            .map_err(|_| Error::Config(format!("invalid {ENV_METRICS_INTERVAL}: {secs}")))?;
    }

    Ok(cfg)
}

fn expand_tilde(raw: &str, home: Option<&str>) -> PathBuf {
    if let Some(home) = home {
        if let Some(rest) = raw.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
        if raw == "~" {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(raw)
}

#[derive(Debug, Deserialize, Default)]
struct TomlConfig {
    server: Option<TomlServer>,
    storage: Option<TomlStorage>,
    log: Option<TomlLog>,
    metrics: Option<TomlMetrics>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlServer {
    bind: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlStorage {
    db_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlLog {
    level: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlMetrics {
    snapshot_interval_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Config {
        Config {
            bind: "127.0.0.1:9100".to_string(),
            db_path: PathBuf::from("/data/memgarden.db"),
            log_level: "info".to_string(),
            metrics_snapshot_interval_secs: 60,
        }
    }

    #[test]
    fn config_precedence() {
        // defaults alone.
        let cfg = from_parts(defaults(), None, &HashMap::new()).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:9100");
        assert_eq!(cfg.log_level, "info");

        // TOML overrides defaults.
        let toml_str = r#"
            [server]
            bind = "0.0.0.0:9200"
            [log]
            level = "debug"
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9200");
        assert_eq!(cfg.log_level, "debug");
        // Untouched by TOML: still default.
        assert_eq!(cfg.metrics_snapshot_interval_secs, 60);

        // Env overrides TOML.
        let mut env = HashMap::new();
        env.insert(ENV_BIND.to_string(), "127.0.0.1:9300".to_string());
        env.insert(ENV_METRICS_INTERVAL.to_string(), "120".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:9300"); // env wins over toml
        assert_eq!(cfg.log_level, "debug"); // toml wins over default (no env override)
        assert_eq!(cfg.metrics_snapshot_interval_secs, 120); // env wins over default
    }

    #[test]
    fn config_malformed_toml_errors() {
        let err = from_parts(defaults(), Some("bind = ["), &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("line"),
            "expected line/col info in error, got: {msg}"
        );
    }

    #[test]
    fn tilde_expansion() {
        let mut env = HashMap::new();
        env.insert(ENV_HOME.to_string(), "/home/testuser".to_string());
        env.insert(ENV_DB_PATH.to_string(), "~/data/memgarden.db".to_string());
        let cfg = from_parts(defaults(), None, &env).unwrap();
        assert_eq!(
            cfg.db_path,
            PathBuf::from("/home/testuser/data/memgarden.db")
        );

        // Bare "~" also expands.
        let mut env2 = HashMap::new();
        env2.insert(ENV_HOME.to_string(), "/home/testuser".to_string());
        env2.insert(ENV_DB_PATH.to_string(), "~".to_string());
        let cfg2 = from_parts(defaults(), None, &env2).unwrap();
        assert_eq!(cfg2.db_path, PathBuf::from("/home/testuser"));

        // No HOME in env -> left as-is.
        let mut env3 = HashMap::new();
        env3.insert(ENV_DB_PATH.to_string(), "~/data/memgarden.db".to_string());
        let cfg3 = from_parts(defaults(), None, &env3).unwrap();
        assert_eq!(cfg3.db_path, PathBuf::from("~/data/memgarden.db"));
    }
}
