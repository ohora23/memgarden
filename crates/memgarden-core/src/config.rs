//! Daemon configuration: struct defaults -> TOML file -> env overrides.

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
const ENV_MODEL_DIR: &str = "MEMGARDEN_MODEL_DIR";
const ENV_EMBED_THREADS: &str = "MEMGARDEN_EMBED_THREADS";
const ENV_OLLAMA_URL: &str = "MEMGARDEN_OLLAMA_URL";
const ENV_OLLAMA_MODEL: &str = "MEMGARDEN_OLLAMA_MODEL";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub bind: String,
    pub db_path: PathBuf,
    pub log_level: String,
    pub metrics_snapshot_interval_secs: u64,
    pub embedding: EmbeddingConfig,
    pub ollama: OllamaConfig,
}

/// `[ollama]` — the local LLM used for fact extraction (CE-5, B2). Loopback
/// HTTP only (`reqwest` has no TLS feature enabled — see the workspace
/// Cargo.toml comment).
#[derive(Debug, Clone, PartialEq)]
pub struct OllamaConfig {
    pub base_url: String,
    pub model: String,
    pub temperature: f64,
    pub num_predict: u32,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub keep_alive: String,
    pub max_concurrent: usize,
}

/// `[embedding]` — in-binary CPU embeddings (CE-4). `intra_threads = 4` and
/// `batch_size = 8` are measured defaults, not arbitrary: see the plan's
/// Verified Environment Facts (4 threads is the throughput/contention
/// sweet spot against Ollama) and Critic Revision R9 (batch 8 caps a single
/// backlog tick's ONNX mutex hold to ~18ms).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub model_dir: PathBuf,
    pub intra_threads: usize,
    pub batch_size: usize,
    pub backlog_poll_secs: u64,
    pub debug_endpoint: bool,
}

impl Config {
    /// Struct defaults: bind 127.0.0.1:9100, db_path = XDG default, log
    /// info, metrics snapshot every 60s, embeddings on.
    pub fn defaults() -> Result<Config> {
        Ok(Config {
            bind: "127.0.0.1:9100".to_string(),
            db_path: paths::default_db_path()?,
            log_level: "info".to_string(),
            metrics_snapshot_interval_secs: 60,
            embedding: EmbeddingConfig {
                enabled: true,
                model_dir: paths::models_dir()?,
                intra_threads: 4,
                batch_size: 8,
                backlog_poll_secs: 5,
                debug_endpoint: false,
            },
            ollama: OllamaConfig {
                base_url: "http://127.0.0.1:11434".to_string(),
                // legacy: the live fork daemon's HINDSIGHT_API_LLM_MODEL — the
                // bare "qwen3-14b" tag does NOT exist on this machine, only
                // "qwen3-14b-nothink:latest" / "qwen3-14b-q6:latest" do
                // (plan Verified Environment Facts, Ollama section).
                model: "qwen3-14b-nothink:latest".to_string(),
                // legacy: config.py:210 DEFAULT_LLM_TEMPERATURE_RETAIN.
                temperature: 0.1,
                // Deliberate divergence from legacy's 64000 (config.py:1094):
                // at the measured ~65 tok/s that's a 16-minute worst case for
                // one chunk. 8192 comfortably covers a 3000-char chunk's
                // facts. See docs/design/ce-5a-ollama-extract.md.
                num_predict: 8192,
                request_timeout_secs: 300,
                // legacy: config.py:862-864 (retry count only — backoff cap
                // differs, see ollama.rs R14).
                max_retries: 3,
                keep_alive: "10m".to_string(),
                // A 14B model sharing one GPU with nothing else must not be
                // hit concurrently.
                max_concurrent: 1,
            },
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
            ENV_MODEL_DIR,
            ENV_EMBED_THREADS,
            ENV_OLLAMA_URL,
            ENV_OLLAMA_MODEL,
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
        if let Some(embedding) = parsed.embedding {
            if let Some(v) = embedding.enabled {
                cfg.embedding.enabled = v;
            }
            if let Some(v) = embedding.model_dir {
                cfg.embedding.model_dir = expand_tilde(&v, home);
            }
            if let Some(v) = embedding.intra_threads {
                cfg.embedding.intra_threads = v;
            }
            if let Some(v) = embedding.batch_size {
                cfg.embedding.batch_size = v;
            }
            if let Some(v) = embedding.backlog_poll_secs {
                cfg.embedding.backlog_poll_secs = v;
            }
            if let Some(v) = embedding.debug_endpoint {
                cfg.embedding.debug_endpoint = v;
            }
        }
        if let Some(ollama) = parsed.ollama {
            if let Some(v) = ollama.base_url {
                cfg.ollama.base_url = v;
            }
            if let Some(v) = ollama.model {
                cfg.ollama.model = v;
            }
            if let Some(v) = ollama.temperature {
                cfg.ollama.temperature = v;
            }
            if let Some(v) = ollama.num_predict {
                cfg.ollama.num_predict = v;
            }
            if let Some(v) = ollama.request_timeout_secs {
                cfg.ollama.request_timeout_secs = v;
            }
            if let Some(v) = ollama.max_retries {
                cfg.ollama.max_retries = v;
            }
            if let Some(v) = ollama.keep_alive {
                cfg.ollama.keep_alive = v;
            }
            if let Some(v) = ollama.max_concurrent {
                cfg.ollama.max_concurrent = v;
            }
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
    if let Some(model_dir) = env.get(ENV_MODEL_DIR) {
        cfg.embedding.model_dir = expand_tilde(model_dir, home);
    }
    if let Some(threads) = env.get(ENV_EMBED_THREADS) {
        cfg.embedding.intra_threads = threads
            .parse()
            .map_err(|_| Error::Config(format!("invalid {ENV_EMBED_THREADS}: {threads}")))?;
    }
    if let Some(url) = env.get(ENV_OLLAMA_URL) {
        cfg.ollama.base_url = url.clone();
    }
    if let Some(model) = env.get(ENV_OLLAMA_MODEL) {
        cfg.ollama.model = model.clone();
    }

    // Fail at startup, not per-request: a typo'd base_url would otherwise
    // surface only as transport errors + a permanently DEGRADED /healthz,
    // and a zero timeout/concurrency wedges the client silently.
    if !cfg.ollama.base_url.starts_with("http://") && !cfg.ollama.base_url.starts_with("https://")
    {
        return Err(Error::Config(format!(
            "ollama.base_url must start with http:// or https://: {}",
            cfg.ollama.base_url
        )));
    }
    if cfg.ollama.request_timeout_secs == 0 {
        return Err(Error::Config(
            "ollama.request_timeout_secs must be > 0".to_string(),
        ));
    }
    if cfg.ollama.max_concurrent == 0 {
        return Err(Error::Config(
            "ollama.max_concurrent must be > 0".to_string(),
        ));
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
    embedding: Option<TomlEmbedding>,
    ollama: Option<TomlOllama>,
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

#[derive(Debug, Deserialize, Default)]
struct TomlEmbedding {
    enabled: Option<bool>,
    model_dir: Option<String>,
    intra_threads: Option<usize>,
    batch_size: Option<usize>,
    backlog_poll_secs: Option<u64>,
    debug_endpoint: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlOllama {
    base_url: Option<String>,
    model: Option<String>,
    temperature: Option<f64>,
    num_predict: Option<u32>,
    request_timeout_secs: Option<u64>,
    max_retries: Option<u32>,
    keep_alive: Option<String>,
    max_concurrent: Option<usize>,
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
            embedding: EmbeddingConfig {
                enabled: true,
                model_dir: PathBuf::from("/data/models"),
                intra_threads: 4,
                batch_size: 8,
                backlog_poll_secs: 5,
                debug_endpoint: false,
            },
            ollama: OllamaConfig {
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen3-14b-nothink:latest".to_string(),
                temperature: 0.1,
                num_predict: 8192,
                request_timeout_secs: 300,
                max_retries: 3,
                keep_alive: "10m".to_string(),
                max_concurrent: 1,
            },
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

    #[test]
    fn embedding_precedence() {
        let toml_str = r#"
            [embedding]
            enabled = false
            batch_size = 16
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert!(!cfg.embedding.enabled);
        assert_eq!(cfg.embedding.batch_size, 16);
        // Untouched by TOML: still default.
        assert_eq!(cfg.embedding.intra_threads, 4);

        let mut env = HashMap::new();
        env.insert(ENV_HOME.to_string(), "/home/testuser".to_string());
        env.insert(ENV_MODEL_DIR.to_string(), "~/models".to_string());
        env.insert(ENV_EMBED_THREADS.to_string(), "8".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(
            cfg.embedding.model_dir,
            PathBuf::from("/home/testuser/models")
        );
        assert_eq!(cfg.embedding.intra_threads, 8);
        // Env doesn't touch batch_size; TOML value survives.
        assert_eq!(cfg.embedding.batch_size, 16);
    }

    #[test]
    fn embed_threads_invalid_env_errors() {
        let mut env = HashMap::new();
        env.insert(ENV_EMBED_THREADS.to_string(), "not-a-number".to_string());
        assert!(from_parts(defaults(), None, &env).is_err());
    }

    #[test]
    fn ollama_validation_rejects_bad_values() {
        let env = HashMap::new();
        let mut bad_url = defaults();
        bad_url.ollama.base_url = "localhost:11434".to_string(); // no scheme
        assert!(from_parts(bad_url, None, &env).is_err());

        let mut zero_timeout = defaults();
        zero_timeout.ollama.request_timeout_secs = 0;
        assert!(from_parts(zero_timeout, None, &env).is_err());

        let mut zero_concurrent = defaults();
        zero_concurrent.ollama.max_concurrent = 0;
        assert!(from_parts(zero_concurrent, None, &env).is_err());
    }

    #[test]
    fn ollama_precedence() {
        let toml_str = r#"
            [ollama]
            model = "qwen3-14b-q6:latest"
            max_retries = 5
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert_eq!(cfg.ollama.model, "qwen3-14b-q6:latest");
        assert_eq!(cfg.ollama.max_retries, 5);
        // Untouched by TOML: still default.
        assert_eq!(cfg.ollama.base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.ollama.temperature, 0.1);

        let mut env = HashMap::new();
        env.insert(
            ENV_OLLAMA_URL.to_string(),
            "http://127.0.0.1:22222".to_string(),
        );
        env.insert(ENV_OLLAMA_MODEL.to_string(), "other-model".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(cfg.ollama.base_url, "http://127.0.0.1:22222"); // env wins over toml
        assert_eq!(cfg.ollama.model, "other-model"); // env wins over toml
        // Env doesn't touch max_retries; TOML value survives.
        assert_eq!(cfg.ollama.max_retries, 5);
    }
}
