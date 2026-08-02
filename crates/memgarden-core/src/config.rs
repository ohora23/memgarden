//! Daemon configuration: struct defaults -> TOML file -> env overrides.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::paths;
use crate::types::FactType;

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
const ENV_RETAIN_MAX_INITIAL: &str = "MEMGARDEN_RETAIN_MAX_INITIAL_MESSAGES";
const ENV_RETAIN_TOOL_CALLS: &str = "MEMGARDEN_RETAIN_TOOL_CALLS";
const ENV_PROFILE: &str = "MEMGARDEN_PROFILE";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub bind: String,
    pub db_path: PathBuf,
    pub log_level: String,
    pub metrics_snapshot_interval_secs: u64,
    pub embedding: EmbeddingConfig,
    pub ollama: OllamaConfig,
    pub retain: RetainConfig,
    pub recall: RecallConfig,
    pub consolidation: ConsolidationConfig,
    pub profile: ProfileConfig,
}

/// `[consolidation]` — fact→observation consolidation (CE-9).
///
/// Every value is legacy's, from `config.py:1147-1171` and `:1298`. One
/// legacy knob is deliberately **not** here: `consolidation_llm_parallelism`
/// (4, `config.py:1165`) is forced to 1, because it assumes a hosted provider
/// and MemGarden runs a single local 14B model behind
/// `ollama.max_concurrent = 1` — a second concurrent group would queue on
/// that semaphore rather than run, so the knob could only ever be a lie.
/// A config for a value that cannot change is worse than no config.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationConfig {
    /// Cosine at or above which a newly created observation is adjudicated
    /// against its nearest existing twin by one focused LLM call
    /// (`DEFAULT_CONSOLIDATION_DEDUP_THRESHOLD`, `config.py:1157`).
    /// **`>= 1.0` disables the whole dedup path** (`consolidator.py:180-182`).
    pub dedup_threshold: f64,
    /// Seconds between background consolidation ticks
    /// (`DEFAULT_CONSOLIDATION_RECONCILE_INTERVAL_SECONDS`, `config.py:1298`).
    /// **`0` disables the background task**, as it does in legacy; manual
    /// `POST /v1/banks/{id}/consolidate` still works.
    pub interval_secs: u64,
    /// Facts loaded per round (`DEFAULT_CONSOLIDATION_BATCH_SIZE`,
    /// `config.py:1149`).
    pub batch_size: usize,
    /// Facts per LLM call (`DEFAULT_CONSOLIDATION_LLM_BATCH_SIZE`,
    /// `config.py:1153`).
    pub llm_batch_size: usize,
    /// Outer attempts at one LLM batch before the batch is skipped
    /// (`DEFAULT_CONSOLIDATION_MAX_ATTEMPTS`, `config.py:1147`).
    pub max_attempts: u32,
    /// Budget for the per-fact recall that pools existing observations
    /// (`DEFAULT_CONSOLIDATION_RECALL_BUDGET`, `config.py:1167`).
    pub recall_budget: String,
    /// Token budget for that recall — how much existing-observation text one
    /// batch prompt may carry (`DEFAULT_CONSOLIDATION_MAX_TOKENS`,
    /// `config.py:1163`). The prompt's hard ceiling is a `const` in
    /// `consolidate::round`, not this.
    pub max_tokens: usize,
}

/// Hard ceiling on `recall.max_tokens` (config and the per-request
/// `maxTokens` override). Well past any sane injection: the whole point of
/// recall is to spend fewer tokens than the memory saves.
pub const MAX_RECALL_TOKENS: usize = 8192;

/// `[recall]` — hybrid retrieval (CE-6, B4). The token budget itself is not
/// here: it is `[profile] recall_budget`, because it is part of the ported
/// profile preset.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallConfig {
    /// Fact types recalled when the request does not say. **Fork
    /// improvement**: legacy's client default is `["observation"]`
    /// (`scripts/lib/config.py:16`), which measurably degraded results —
    /// the live user overrode it to all three (docs/measurement.md,
    /// memcompare findings), so that override is the server default here.
    pub types: Vec<FactType>,
    /// Results asked of each arm, and the cap on what recall returns.
    /// Over-fetch is derived from it (`max(limit*5, 100)`,
    /// `engine/search/retrieval.py:225`).
    pub limit: usize,
    /// Token ceiling on the recalled text actually returned — the fork
    /// hook's `recallMaxTokens` (`scripts/lib/config.py:15`, passed at
    /// `scripts/recall.py:190`), overridable per request as `maxTokens`.
    ///
    /// Deliberately NOT the same knob as `[profile] recall_budget`: legacy
    /// sends both, `budget` steering how many candidates get reranked
    /// (`rerank_limit = thinking_budget * 2`) and `max_tokens` capping the
    /// injection. Collapsing them made `budget = "low"` cut the injection to
    /// 100 tokens, which would have invalidated the AC-1 A/B against the
    /// live fork (whose coding profile sends `low` *and* 1024).
    pub max_tokens: usize,
    /// Per-arm truncation applied *before* fusion (`engine/search/fusion.py:8`)
    /// so one over-expanding arm cannot crowd out the others. `0` disables,
    /// matching legacy's default (`config.py:940`).
    pub cap_per_source: usize,
    /// Text placed between the `<memgarden_memories>` open tag and the
    /// "Current time" line of `injected_text`; the fork's
    /// `recallPromptPreamble`, moved server-side because MemGarden builds
    /// the injection (plan §Workspace decision keeps the Phase C hook thin).
    pub preamble: String,
}

/// `[retain]` — transcript ingest (CE-5b, B3). Every cap here lives
/// server-side in MemGarden even though the hindsight fork applies them in
/// its Python hook: the `retain_cap_saving` ledger row is a store concern,
/// and the PRD budgets the Phase C hook at <10ms total, so the hook posts a
/// raw transcript and does nothing else (plan decision #4).
#[derive(Debug, Clone, PartialEq)]
pub struct RetainConfig {
    /// Backfill cap. Applies ONLY to a session's first (initial) retain and
    /// keeps the LAST N messages; `0` disables it. legacy fork:
    /// `scripts/lib/config.py:40` `retainMaxInitialMessages`,
    /// `scripts/retain.py:141-147`. This exists because a 102MB legacy
    /// transcript blew the server's retain wall-clock limit.
    pub max_initial_messages: usize,
    /// legacy: `config.py:1095` `retain_chunk_size`.
    pub chunk_size: usize,
    /// Per-string-field cap inside a `tool_use` input
    /// (`scripts/lib/content.py:413`).
    pub tool_input_field_max: usize,
    /// Serialized-whole cap for a `tool_use` input; above it only the
    /// priority keys survive (`scripts/lib/content.py:417`).
    pub tool_input_total_max: usize,
    /// `tool_result` content truncation (`scripts/lib/content.py:299-300`).
    pub tool_result_max: usize,
    /// Max `file:<path>` tags per retain (`scripts/retain.py:237-241`).
    pub file_tag_cap: usize,
    /// Bounded worker queue; a full queue answers 429 rather than growing
    /// unboundedly in RAM.
    pub queue_capacity: usize,
    /// Per-job wall clock (Critic Revision R11) — parity with the live
    /// hindsight daemon's `RETAIN_WALL_TIMEOUT=7200`. Exceeding it marks the
    /// job `failed` with the partial progress recorded.
    pub wall_timeout_secs: u64,
    /// Whether tool calls are retained at all. legacy default is `false`;
    /// the `coding` profile flips it to `true` (that is the whole point of
    /// the two tool-input caps).
    pub include_tool_calls: bool,
}

/// `[profile]` — named presets that fill in grouped defaults for a usage
/// pattern, ported from the fork's `PROFILE_PRESETS`
/// (`scripts/lib/config.py:74-99`). Precedence matches legacy
/// (`:206-223`): built-in defaults -> TOML -> env -> **preset fills only the
/// keys the user did not set explicitly**.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    /// `""` = no preset. Only `"coding"` exists today.
    pub name: String,
    /// Default bank mission for banks created without one. Per-bank
    /// overrides live in `banks.mission`; no new column (plan §PR B3).
    pub bank_mission: String,
    /// Default extraction mission. Per-bank override:
    /// `banks.disposition` JSON `{"retain_mission": ...}`.
    pub retain_mission: String,
    /// `low` | `mid` | `high`. Consumed by CE-6 (B4); carried here now
    /// because it is part of the ported preset.
    pub recall_budget: String,
}

/// The `coding` preset, verbatim from `scripts/lib/config.py:80-98`. The two
/// mission strings must not be reworded — they are the live fork's and AC-1
/// compares extraction quality against it.
const CODING_BANK_MISSION: &str = "You are a coding assistant with long-term memory of this project's engineering history: decisions, bug fixes, conventions, and workflows.";
const CODING_RETAIN_MISSION: &str = "Extract durable engineering knowledge: technical decisions and their rationale, bug root causes and their fixes, architecture and API constraints, commands that worked for building/testing/running, code style and workflow preferences, and file- or module-specific gotchas. Ignore greetings, routine tool output, and transient operational chatter.";

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
            retain: RetainConfig {
                max_initial_messages: 300,
                chunk_size: 3000,
                tool_input_field_max: 300,
                tool_input_total_max: 1500,
                tool_result_max: 2000,
                file_tag_cap: 20,
                queue_capacity: 32,
                wall_timeout_secs: 7200,
                include_tool_calls: false,
            },
            recall: RecallConfig {
                types: vec![FactType::World, FactType::Observation, FactType::Experience],
                limit: 20,
                max_tokens: 1024,
                cap_per_source: 0,
                preamble: String::new(),
            },
            consolidation: ConsolidationConfig {
                dedup_threshold: 0.97,
                interval_secs: 300,
                batch_size: 50,
                llm_batch_size: 8,
                max_attempts: 3,
                recall_budget: "low".to_string(),
                max_tokens: 512,
            },
            profile: ProfileConfig {
                name: String::new(),
                bank_mission: String::new(),
                retain_mission: String::new(),
                recall_budget: "mid".to_string(),
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
            ENV_RETAIN_MAX_INITIAL,
            ENV_RETAIN_TOOL_CALLS,
            ENV_PROFILE,
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
    // Keys the user set explicitly (TOML or env). A profile preset must not
    // override these — legacy `scripts/lib/config.py:219-222`.
    let mut explicit: Vec<&'static str> = Vec::new();

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
        if let Some(retain) = parsed.retain {
            if let Some(v) = retain.max_initial_messages {
                cfg.retain.max_initial_messages = v;
            }
            if let Some(v) = retain.chunk_size {
                cfg.retain.chunk_size = v;
            }
            if let Some(v) = retain.tool_input_field_max {
                cfg.retain.tool_input_field_max = v;
            }
            if let Some(v) = retain.tool_input_total_max {
                cfg.retain.tool_input_total_max = v;
            }
            if let Some(v) = retain.tool_result_max {
                cfg.retain.tool_result_max = v;
            }
            if let Some(v) = retain.file_tag_cap {
                cfg.retain.file_tag_cap = v;
            }
            if let Some(v) = retain.queue_capacity {
                cfg.retain.queue_capacity = v;
            }
            if let Some(v) = retain.wall_timeout_secs {
                cfg.retain.wall_timeout_secs = v;
            }
            if let Some(v) = retain.include_tool_calls {
                cfg.retain.include_tool_calls = v;
                explicit.push("include_tool_calls");
            }
        }
        if let Some(recall) = parsed.recall {
            if let Some(v) = recall.types {
                cfg.recall.types = v;
            }
            if let Some(v) = recall.limit {
                cfg.recall.limit = v;
            }
            if let Some(v) = recall.max_tokens {
                cfg.recall.max_tokens = v;
            }
            if let Some(v) = recall.cap_per_source {
                cfg.recall.cap_per_source = v;
            }
            if let Some(v) = recall.preamble {
                cfg.recall.preamble = v;
            }
        }
        if let Some(c) = parsed.consolidation {
            if let Some(v) = c.dedup_threshold {
                cfg.consolidation.dedup_threshold = v;
            }
            if let Some(v) = c.interval_secs {
                cfg.consolidation.interval_secs = v;
            }
            if let Some(v) = c.batch_size {
                cfg.consolidation.batch_size = v;
            }
            if let Some(v) = c.llm_batch_size {
                cfg.consolidation.llm_batch_size = v;
            }
            if let Some(v) = c.max_attempts {
                cfg.consolidation.max_attempts = v;
            }
            if let Some(v) = c.recall_budget {
                cfg.consolidation.recall_budget = v;
            }
            if let Some(v) = c.max_tokens {
                cfg.consolidation.max_tokens = v;
            }
        }
        if let Some(profile) = parsed.profile {
            if let Some(v) = profile.name {
                cfg.profile.name = v;
            }
            if let Some(v) = profile.bank_mission {
                cfg.profile.bank_mission = v;
                explicit.push("bank_mission");
            }
            if let Some(v) = profile.retain_mission {
                cfg.profile.retain_mission = v;
                explicit.push("retain_mission");
            }
            if let Some(v) = profile.recall_budget {
                cfg.profile.recall_budget = v;
                explicit.push("recall_budget");
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
    if let Some(raw) = env.get(ENV_RETAIN_MAX_INITIAL) {
        cfg.retain.max_initial_messages = raw
            .parse()
            .map_err(|_| Error::Config(format!("invalid {ENV_RETAIN_MAX_INITIAL}: {raw}")))?;
    }
    if let Some(raw) = env.get(ENV_RETAIN_TOOL_CALLS) {
        // Same truthy set as the fork's `_cast_env` (`lib/config.py:136`).
        cfg.retain.include_tool_calls =
            matches!(raw.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
        explicit.push("include_tool_calls");
    }
    if let Some(name) = env.get(ENV_PROFILE) {
        cfg.profile.name = name.clone();
    }

    // Profile preset, applied LAST and only to keys nobody set explicitly.
    if !cfg.profile.name.is_empty() {
        match cfg.profile.name.as_str() {
            "coding" => {
                if !explicit.contains(&"include_tool_calls") {
                    cfg.retain.include_tool_calls = true;
                }
                if !explicit.contains(&"bank_mission") {
                    cfg.profile.bank_mission = CODING_BANK_MISSION.to_string();
                }
                if !explicit.contains(&"retain_mission") {
                    cfg.profile.retain_mission = CODING_RETAIN_MISSION.to_string();
                }
                if !explicit.contains(&"recall_budget") {
                    cfg.profile.recall_budget = "low".to_string();
                }
            }
            // Legacy only warns on stderr here (`lib/config.py:214-217`).
            // MemGarden fails at startup instead, matching the [ollama]
            // validation below: a typo'd profile silently running with the
            // wrong missions is worse than a refused boot.
            other => {
                return Err(Error::Config(format!(
                    "unknown profile.name '{other}' — valid: coding"
                )));
            }
        }
    }

    if !matches!(cfg.profile.recall_budget.as_str(), "low" | "mid" | "high") {
        return Err(Error::Config(format!(
            "profile.recall_budget must be low|mid|high: {}",
            cfg.profile.recall_budget
        )));
    }
    if cfg.recall.types.is_empty() {
        return Err(Error::Config(
            "recall.types must list at least one fact type".to_string(),
        ));
    }
    if cfg.recall.limit == 0 || cfg.recall.limit > 200 {
        return Err(Error::Config(format!(
            "recall.limit must be 1..=200: {}",
            cfg.recall.limit
        )));
    }
    if !(1..=MAX_RECALL_TOKENS).contains(&cfg.recall.max_tokens) {
        return Err(Error::Config(format!(
            "recall.max_tokens must be 1..={MAX_RECALL_TOKENS}: {}",
            cfg.recall.max_tokens
        )));
    }
    // The knob that decides whether a 14B model is called at all. Below the
    // threshold there is no probe and no call; at 0.0 (or negative) every
    // candidate clears it, so **every** observation created fires an
    // adjudication against its nearest neighbour however unrelated — 2.1s of
    // GPU each, serialised behind `ollama.max_concurrent = 1`, which is a
    // batch round stalled for minutes. 0.5 is already far below anything
    // defensible as "near-duplicate"; 1.0 is inclusive because `>= 1.0` is
    // the documented way to disable the path.
    if !(0.5..=1.0).contains(&cfg.consolidation.dedup_threshold) {
        return Err(Error::Config(format!(
            "consolidation.dedup_threshold must be 0.5..=1.0 (1.0 disables dedup): {}",
            cfg.consolidation.dedup_threshold
        )));
    }
    // A round with no facts, no LLM batch, or no attempt is a background task
    // that burns a tick and does nothing — and `batch_size = 0` in particular
    // would leave the watermark frozen forever with no error anywhere.
    for (name, value) in [
        ("consolidation.batch_size", cfg.consolidation.batch_size),
        (
            "consolidation.llm_batch_size",
            cfg.consolidation.llm_batch_size,
        ),
        (
            "consolidation.max_attempts",
            cfg.consolidation.max_attempts as usize,
        ),
    ] {
        if value == 0 {
            return Err(Error::Config(format!("{name} must be > 0")));
        }
    }
    if !matches!(
        cfg.consolidation.recall_budget.as_str(),
        "low" | "mid" | "high"
    ) {
        return Err(Error::Config(format!(
            "consolidation.recall_budget must be low|mid|high: {}",
            cfg.consolidation.recall_budget
        )));
    }
    // Same ceiling the recall route enforces on `maxTokens`: this value is
    // that parameter, on the consolidation path.
    if !(1..=MAX_RECALL_TOKENS).contains(&cfg.consolidation.max_tokens) {
        return Err(Error::Config(format!(
            "consolidation.max_tokens must be 1..={MAX_RECALL_TOKENS}: {}",
            cfg.consolidation.max_tokens
        )));
    }
    if cfg.retain.chunk_size == 0 {
        return Err(Error::Config("retain.chunk_size must be > 0".to_string()));
    }
    if cfg.retain.queue_capacity == 0 {
        return Err(Error::Config(
            "retain.queue_capacity must be > 0".to_string(),
        ));
    }
    if cfg.retain.wall_timeout_secs == 0 {
        return Err(Error::Config(
            "retain.wall_timeout_secs must be > 0".to_string(),
        ));
    }

    // Fail at startup, not per-request: a typo'd base_url would otherwise
    // surface only as transport errors + a permanently DEGRADED /healthz,
    // and a zero timeout/concurrency wedges the client silently.
    if !cfg.ollama.base_url.starts_with("http://") && !cfg.ollama.base_url.starts_with("https://") {
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
    retain: Option<TomlRetain>,
    recall: Option<TomlRecall>,
    consolidation: Option<TomlConsolidation>,
    profile: Option<TomlProfile>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlConsolidation {
    dedup_threshold: Option<f64>,
    interval_secs: Option<u64>,
    batch_size: Option<usize>,
    llm_batch_size: Option<usize>,
    max_attempts: Option<u32>,
    recall_budget: Option<String>,
    max_tokens: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlRetain {
    max_initial_messages: Option<usize>,
    chunk_size: Option<usize>,
    tool_input_field_max: Option<usize>,
    tool_input_total_max: Option<usize>,
    tool_result_max: Option<usize>,
    file_tag_cap: Option<usize>,
    queue_capacity: Option<usize>,
    wall_timeout_secs: Option<u64>,
    include_tool_calls: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlRecall {
    types: Option<Vec<FactType>>,
    limit: Option<usize>,
    max_tokens: Option<usize>,
    cap_per_source: Option<usize>,
    preamble: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlProfile {
    name: Option<String>,
    bank_mission: Option<String>,
    retain_mission: Option<String>,
    recall_budget: Option<String>,
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
            retain: RetainConfig {
                max_initial_messages: 300,
                chunk_size: 3000,
                tool_input_field_max: 300,
                tool_input_total_max: 1500,
                tool_result_max: 2000,
                file_tag_cap: 20,
                queue_capacity: 32,
                wall_timeout_secs: 7200,
                include_tool_calls: false,
            },
            recall: RecallConfig {
                types: vec![FactType::World, FactType::Observation, FactType::Experience],
                limit: 20,
                max_tokens: 1024,
                cap_per_source: 0,
                preamble: String::new(),
            },
            consolidation: ConsolidationConfig {
                dedup_threshold: 0.97,
                interval_secs: 300,
                batch_size: 50,
                llm_batch_size: 8,
                max_attempts: 3,
                recall_budget: "low".to_string(),
                max_tokens: 512,
            },
            profile: ProfileConfig {
                name: String::new(),
                bank_mission: String::new(),
                retain_mission: String::new(),
                recall_budget: "mid".to_string(),
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

    /// The knob decides whether a 14B model is called at all, so an
    /// out-of-range value is a GPU-cost bug, not a taste question. 1.0 must
    /// stay legal — it is the documented way to switch dedup off.
    #[test]
    fn consolidation_dedup_threshold_is_range_checked() {
        let env = HashMap::new();
        assert_eq!(defaults().consolidation.dedup_threshold, 0.97);

        for bad in [0.0, -1.0, 0.49, 1.01, f64::NAN] {
            let mut cfg = defaults();
            cfg.consolidation.dedup_threshold = bad;
            assert!(
                from_parts(cfg, None, &env).is_err(),
                "dedup_threshold {bad} must be rejected"
            );
        }
        for ok in [0.5, 0.97, 1.0] {
            let mut cfg = defaults();
            cfg.consolidation.dedup_threshold = ok;
            assert!(from_parts(cfg, None, &env).is_ok(), "{ok} must be accepted");
        }

        let cfg = from_parts(
            defaults(),
            Some("[consolidation]\ndedup_threshold = 1.0\n"),
            &env,
        )
        .unwrap();
        assert_eq!(cfg.consolidation.dedup_threshold, 1.0);
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

    #[test]
    fn retain_defaults_match_the_fork() {
        let cfg = from_parts(defaults(), None, &HashMap::new()).unwrap();
        assert_eq!(cfg.retain.max_initial_messages, 300);
        assert_eq!(cfg.retain.chunk_size, 3000);
        assert_eq!(cfg.retain.tool_input_field_max, 300);
        assert_eq!(cfg.retain.tool_input_total_max, 1500);
        assert_eq!(cfg.retain.tool_result_max, 2000);
        assert_eq!(cfg.retain.file_tag_cap, 20);
        assert_eq!(cfg.retain.wall_timeout_secs, 7200);
        assert!(!cfg.retain.include_tool_calls, "legacy default is false");
    }

    #[test]
    fn retain_precedence_toml_then_env() {
        let toml_str = r#"
            [retain]
            max_initial_messages = 50
            chunk_size = 1000
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert_eq!(cfg.retain.max_initial_messages, 50);
        assert_eq!(cfg.retain.chunk_size, 1000);

        let mut env = HashMap::new();
        env.insert(ENV_RETAIN_MAX_INITIAL.to_string(), "0".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(cfg.retain.max_initial_messages, 0, "env wins; 0 = disabled");
        assert_eq!(cfg.retain.chunk_size, 1000);
    }

    #[test]
    fn coding_profile_fills_only_unset_keys() {
        let mut env = HashMap::new();
        env.insert(ENV_PROFILE.to_string(), "coding".to_string());
        let cfg = from_parts(defaults(), None, &env).unwrap();
        assert!(cfg.retain.include_tool_calls);
        assert_eq!(cfg.profile.recall_budget, "low");
        assert!(cfg.profile.retain_mission.starts_with("Extract durable"));
        assert!(cfg.profile.bank_mission.starts_with("You are a coding"));

        // Explicit TOML values survive the preset (legacy config.py:219-222).
        let toml_str = r#"
            [retain]
            include_tool_calls = false
            [profile]
            name = "coding"
            recall_budget = "high"
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert!(!cfg.retain.include_tool_calls, "explicit TOML beats preset");
        assert_eq!(cfg.profile.recall_budget, "high");
        // Not set explicitly -> preset still fills it.
        assert!(cfg.profile.retain_mission.starts_with("Extract durable"));
    }

    #[test]
    fn unknown_profile_and_bad_retain_values_are_rejected() {
        let mut env = HashMap::new();
        env.insert(ENV_PROFILE.to_string(), "nope".to_string());
        assert!(from_parts(defaults(), None, &env).is_err());

        let mut zero_chunk = defaults();
        zero_chunk.retain.chunk_size = 0;
        assert!(from_parts(zero_chunk, None, &HashMap::new()).is_err());

        let mut zero_queue = defaults();
        zero_queue.retain.queue_capacity = 0;
        assert!(from_parts(zero_queue, None, &HashMap::new()).is_err());

        let mut bad_budget = defaults();
        bad_budget.profile.recall_budget = "enormous".to_string();
        assert!(from_parts(bad_budget, None, &HashMap::new()).is_err());

        let mut no_types = defaults();
        no_types.recall.types = vec![];
        assert!(from_parts(no_types, None, &HashMap::new()).is_err());

        let mut bad_limit = defaults();
        bad_limit.recall.limit = 0;
        assert!(from_parts(bad_limit, None, &HashMap::new()).is_err());
        let mut huge_limit = defaults();
        huge_limit.recall.limit = 5000;
        assert!(from_parts(huge_limit, None, &HashMap::new()).is_err());

        let mut zero_tokens = defaults();
        zero_tokens.recall.max_tokens = 0;
        assert!(from_parts(zero_tokens, None, &HashMap::new()).is_err());
        let mut huge_tokens = defaults();
        huge_tokens.recall.max_tokens = MAX_RECALL_TOKENS + 1;
        assert!(from_parts(huge_tokens, None, &HashMap::new()).is_err());
    }

    #[test]
    fn recall_defaults_and_toml() {
        let cfg = from_parts(defaults(), None, &HashMap::new()).unwrap();
        assert_eq!(
            cfg.recall.types,
            vec![FactType::World, FactType::Observation, FactType::Experience],
            "server default is all three, NOT legacy's observation-only client default"
        );
        assert_eq!(cfg.recall.limit, 20);
        assert_eq!(
            cfg.recall.max_tokens, 1024,
            "fork parity: scripts/lib/config.py:15 recallMaxTokens"
        );
        assert_eq!(
            cfg.recall.cap_per_source, 0,
            "0 = disabled, legacy config.py:940"
        );
        assert_eq!(cfg.recall.preamble, "");

        let toml_str = r#"
            [recall]
            types = ["observation"]
            limit = 5
            max_tokens = 256
            cap_per_source = 40
            preamble = "Relevant memories:"
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert_eq!(cfg.recall.types, vec![FactType::Observation]);
        assert_eq!(cfg.recall.limit, 5);
        assert_eq!(cfg.recall.max_tokens, 256);
        assert_eq!(cfg.recall.cap_per_source, 40);
        assert_eq!(cfg.recall.preamble, "Relevant memories:");

        // An invalid fact type is a startup error, not a silent drop.
        assert!(
            from_parts(
                defaults(),
                Some("[recall]\ntypes = [\"nope\"]"),
                &HashMap::new()
            )
            .is_err()
        );
    }
}
