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
/// Truthy -> `[hooks] enabled = false`. The hook binary also short-circuits on
/// this **before** loading any config (`memgarden_cli::dispatch`), so a user
/// who wants the hooks off does not pay a TOML read to find out.
pub const ENV_HOOKS_DISABLE: &str = "MEMGARDEN_HOOKS_DISABLE";
/// Overrides `[hooks] daemon_url`. Exists mainly so the bench harness and the
/// integration tests can point the CLI at an ephemeral port without writing a
/// config file.
pub const ENV_DAEMON_URL: &str = "MEMGARDEN_DAEMON_URL";

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
    pub reranker: RerankerConfig,
    pub consolidation: ConsolidationConfig,
    pub mental: MentalConfig,
    pub profile: ProfileConfig,
    pub hooks: HooksConfig,
}

/// `[hooks]` — the Claude Code integration (Phase C). Read by **two**
/// binaries: `memgarden` (the hook CLI, on every invocation) and `memgardend`
/// (only `session_retention_days`, for the `sessions` GC). It lives in core
/// for exactly that reason.
///
/// Every timeout here is a *client* timeout. They are deliberately unequal —
/// see the per-field comments — because the daemon's work behind each endpoint
/// differs by four orders of magnitude, and a single number would either wedge
/// a prompt for seconds or abandon a legitimate retain.
#[derive(Debug, Clone, PartialEq)]
pub struct HooksConfig {
    /// Master switch. `false` (or `MEMGARDEN_HOOKS_DISABLE=1`) makes every
    /// subcommand a no-op that still exits 0.
    pub enabled: bool,
    /// `shadow` | `full`. **`shadow` is the default and injects nothing**
    /// (plan §Binding decisions #13): installing the switch must not throw it.
    pub mode: String,
    /// Loopback only — `src/http.rs` refuses anything that is not
    /// `127.0.0.1`/`localhost`/`::1`, and the daemon's `check_host` would 403
    /// it anyway (`middleware.rs:34-46`).
    pub daemon_url: String,
    /// Loopback `connect()` completes in microseconds; 50ms is three orders of
    /// magnitude of headroom and still bounds a daemon that is listening but
    /// not accepting.
    pub connect_timeout_ms: u64,
    /// ~10x the measured loaded recall p95. The turn proceeds with no memories
    /// past this — recall fails open.
    pub recall_timeout_ms: u64,
    /// Much larger than recall's on purpose: `POST /v1/retain`'s `prepare()`
    /// is synchronous before the 202 (tokenize twice, upsert the document,
    /// insert the ledger and job rows) and was observed at ~0.6s on a 9.4MB
    /// initial retain.
    pub retain_timeout_ms: u64,
    /// legacy: `lib/config.py:34` `retainEveryNTurns`.
    pub retain_every_n_turns: u64,
    /// Ceiling on the recall query, in **characters** (`recall.py:167`,
    /// `lib/config.py:19` `recallMaxQueryChars`).
    ///
    /// Characters rather than bytes because that is what legacy slices and
    /// what the daemon's `MIN_QUERY_CHARS` counts — and the two units diverge
    /// violently on the Korean this repo is measured against. The upper bound
    /// in `validate` is arithmetic, not taste: a char is at most 4 UTF-8
    /// bytes, so `2048` is the largest value that cannot produce a query over
    /// the daemon's `MAX_QUERY_BYTES` (8 KB, `routes/recall.rs`) — above it a
    /// hook could 400 on every prompt with a perfectly valid config.
    pub recall_max_query_chars: usize,
    /// Consecutive transport failures before the circuit breaker opens.
    pub breaker_failures: u32,
    /// How long it stays open. A hung daemon then costs
    /// `breaker_failures x recall_timeout_ms` per cooldown instead of
    /// `recall_timeout_ms` per prompt.
    pub breaker_cooldown_secs: u64,
    /// Durable 4xx rejections before a session is marked poisoned.
    pub max_reject_failures: u32,
    /// Poisoning is a slow-retry state, not a latch: a poisoned session still
    /// retries this often, and any success clears it.
    pub poison_retry_secs: u64,
    /// How many stale sessions the detached `hook catchup` child (C2b) works
    /// through per launch. `0` disables catch-up **selection** but not the
    /// child, which also collects the state directory: a knob meaning "catch
    /// up on fewer sessions" must not also mean "leak one state file per
    /// session, forever".
    pub catchup_max_sessions: usize,
    /// How long a session outlives its last sighting, on **both** sides:
    /// `memgardend`'s metrics tick collects the `sessions` row, and the
    /// detached catch-up child collects the local state file with the same
    /// window. One number, so a row and its cache never disagree about
    /// whether a session still exists.
    pub session_retention_days: u64,
    /// Ceiling on a retain POST body. Must stay at or under the daemon's
    /// `MAX_RETAIN_BODY_BYTES` (32MB, `routes/retain.rs:36`), which is
    /// validated below — a larger value could only ever produce a 413.
    pub max_post_bytes: usize,
    /// Ceiling on what `hook recall` will put on stdout as model context.
    pub max_inject_bytes: usize,
    /// Ceiling on the shadow-mode log before it rotates (C3).
    pub shadow_log_max_bytes: u64,
    /// Where the per-session state cache files live. Default `<data>/hooks`.
    pub state_dir: PathBuf,
    /// Diagnostics on stderr. **Never changes an exit code** — legacy's
    /// equivalent flag is exactly what makes `recall.py:287-291` exit 2 and
    /// erase the user's prompt. That is not theoretical: the live
    /// `~/.hindsight/claude-code.json` had `debug: true` until 2026-08-03.
    pub debug: bool,

    // --- bank derivation (plan §Binding decisions #4) ---
    //
    // These three are NOT in the plan's §C2a `[hooks]` key list, which names
    // only the transport/failure knobs — but the same section requires
    // `directoryBankMap` -> static -> `agent::project` resolution, which
    // cannot be expressed without them. Added here rather than invented in
    // `memgarden-cli`, so both binaries see one config surface.
    /// Non-empty pins **every** session to this bank id verbatim: legacy's
    /// `dynamicBankId = false` + `bankId` (`bank.py:103-106`), collapsed into
    /// one knob because the two-knob form has an unreachable combination
    /// (`dynamicBankId = false` with no `bankId` is just the default).
    pub bank_id: String,
    /// The `agent` segment of a dynamic bank id (`bank.py:124`).
    pub agent_name: String,
    /// Exact directory -> bank id overrides, highest precedence
    /// (`bank.py:87-101`). Empty by default, and the lookup is skipped
    /// entirely when empty — each entry costs a `canonicalize()` syscall.
    pub directory_bank_map: HashMap<String, String>,
}

/// The daemon's `MAX_RETAIN_BODY_BYTES` (`routes/retain.rs:36`), mirrored here
/// only to bound `[hooks] max_post_bytes`. A hook that posts more than the
/// server accepts cannot succeed, so this is a config error, not a runtime one.
pub const DAEMON_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Ceiling on `[hooks] max_inject_bytes`, mirroring the hook client's
/// `http::MAX_RESPONSE_BYTES`.
///
/// The unit needs saying, because two reviewers read it two ways:
/// `max_inject_bytes` bounds the **un-escaped** `injected_text` — the thing
/// that reaches the model's context, which is what the name refers to — and
/// **not** the serialized `additionalContext` line, which JSON escaping
/// inflates (measured **6.0×** on a worst case: 65,536 raw → 393,299 bytes on
/// the wire). Bounding the raw value is correct; bounding the line would bound
/// the wrong thing.
///
/// That inflation is exactly why the knob needs an upper bound as well as a
/// lower one. Without this, `max_inject_bytes = 8388608` is accepted and
/// produces a ~48 MB single line that Claude Code must buffer and parse. The
/// response body cannot exceed `MAX_RESPONSE_BYTES` anyway, so anything above
/// it is unreachable config — the same shape as `max_post_bytes` against
/// `DAEMON_MAX_BODY_BYTES`.
pub const MAX_INJECT_BYTES_CEILING: usize = 8 * 1024 * 1024;

/// Ceiling on `[hooks] recall_max_query_chars`, derived rather than chosen:
/// the daemon refuses a query over 8 KB (`routes/recall.rs` `MAX_QUERY_BYTES`)
/// and a `char` is at most 4 UTF-8 bytes, so this is the largest character
/// budget that cannot produce a 400. Same shape as `DAEMON_MAX_BODY_BYTES`
/// above — a client cap that must stay inside a server cap — with a unit
/// conversion in the middle, which is the part that would be easy to get
/// wrong twice.
pub const MAX_RECALL_QUERY_CHARS: usize = 8 * 1024 / 4;

/// `[reranker]` — the embedded ms-marco cross-encoder (CE-11).
///
/// **Off by default, and off *is* parity.** The live legacy daemon runs with
/// `HINDSIGHT_API_RERANKER_PROVIDER=rrf` (`/proc/<pid>/environ`), adopted as
/// part of its 830ms → 20ms latency fix, so a disabled cross-encoder is what
/// the system being matched actually does. It is not a reduction.
///
/// Measured cost is ~1.5–2.6ms **per candidate** on this machine's CPU
/// against AC-2's 35ms p50 for the *whole* recall, which is why `top_k` is 10
/// rather than legacy's `thinking_budget * 2` = 600
/// (`memory_engine.py:5266`). See `docs/design/ce-11-reranker.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankerConfig {
    pub enabled: bool,
    /// Hugging Face repo id. `Xenova/ms-marco-MiniLM-L-6-v2` is the ONNX
    /// export of `cross-encoder/ms-marco-MiniLM-L-6-v2`, which is the exact
    /// model legacy loads (`engine/cross_encoder.py:103,131`). fastembed has
    /// no built-in entry for it, so it is fetched by repo id and loaded
    /// through the user-defined path.
    pub model: String,
    /// How many of the RRF-ordered candidates the cross-encoder sees. The
    /// tail is **dropped**, exactly as legacy drops everything past
    /// `rerank_limit` before reranking — so this is also a cap on what recall
    /// returns whenever it is below `[recall] limit`.
    pub top_k: usize,
    /// ONNX Runtime intra-op threads, same reasoning as
    /// `[embedding] intra_threads`.
    pub threads: usize,
    /// Query-document pairs per ONNX forward pass.
    pub batch_size: usize,
}

/// The pinned default. `rerank.rs` verifies a revision and five SHA-256
/// digests **only** for this exact string, and warns loudly for anything else
/// — `[reranker] model` is the daemon's one operator-settable "which remote
/// artifact do we execute" knob, so the comparison lives next to the value.
pub const DEFAULT_RERANK_MODEL: &str = "Xenova/ms-marco-MiniLM-L-6-v2";

/// `top_k` above this logs a startup warning naming the measured cost. Not a
/// hard error: an operator running a deliberate AC-1 quality experiment has a
/// legitimate reason to raise it, and refusing to boot over a latency
/// preference would be the wrong call. The warning exists so a future config
/// edit cannot blow the SLO *silently*.
///
/// **This threshold is about latency only, and 20 is not a "safe" depth.**
/// CE-11 measured `top_k = 20` and rejected it: it wins nDCG@10, recall@10 and
/// the identifier stratum, but loses MRR decisively (0.739 -> 0.606 over 13
/// queries) at twice the cost. A depth between 11 and 20 is un-measured and
/// gets no warning — see `docs/design/ce-11-reranker.md` before raising this.
pub const RERANK_TOP_K_WARN_ABOVE: usize = 20;

/// Hard ceiling on `reranker.top_k`, matching `recall.limit`'s: reranking
/// deeper than recall can return is pure latency for no ranking change.
pub const MAX_RERANK_TOP_K: usize = 200;

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
pub struct MentalConfig {
    /// Seconds between background mental-model refresh ticks. **`0` disables
    /// the background task**, which was the shipped behaviour for CE-10's
    /// whole life; `POST /v1/banks/{id}/mental-models/{mm_id}/refresh` still
    /// works either way.
    ///
    /// Default 600. A refresh is one LLM call per *due* model, and dueness is
    /// the model's own cron expression — the tick only asks who is due, so the
    /// interval bounds latency-to-due, not GPU spend. Ten minutes is short
    /// enough that an hourly trigger is honoured to the minute and long enough
    /// that a tick costs nothing when nothing is due.
    pub refresh_interval_secs: u64,
}

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
    /// Weight of the semantic boost, the one term that is **not** legacy's.
    /// `0.0` is exact legacy scoring. See
    /// `recall::scoring::combined_with_semantic` for why it exists and what
    /// it was measured against.
    pub semantic_alpha: f64,
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
    /// CE-12. Whether extraction is shown the bank's nearest existing facts
    /// and allowed to declare that a new one retracts them.
    ///
    /// **Off**, and measured that way rather than assumed. On a 7-chunk set
    /// built from this project's own recorded retractions, the detector found
    /// 2 of 3 reachable targets and named **14 facts that retract nothing**,
    /// including all 12 candidates it was shown for one chunk. Requiring it to
    /// quote the false span drove false positives to 0 and detections to 0 as
    /// well. Full numbers and all three arms:
    /// `docs/evidence/supersession-detection.md`.
    ///
    /// The knob stays because the mechanism it feeds — `superseded_by`, the
    /// `hydrate` filter, `nodes::mark_superseded` — is sound and tested, and
    /// because the next attempt at detection should not have to rebuild the
    /// harness. Turning it on costs one embedding + one KNN per chunk and
    /// ~12 lines of prompt; no extra LLM call.
    pub detect_supersession: bool,
    /// CE-12. How many existing facts to show. 12 is the largest that fits
    /// the chunk prompt without displacing the guidelines; a KNN over a
    /// 3,000-character chunk gets vague past roughly that many anyway.
    pub supersession_candidates: usize,
    /// Whether a finished retain job also writes the bank's `task_ledger`
    /// row: the current goal, what is done, what is open, the next action.
    ///
    /// **On**, because the row exists to be looked at. Nothing reads the
    /// table yet — the read path is deliberately unbuilt until the stored
    /// content has been judged, and it cannot be judged unless it is written.
    ///
    /// It costs **one extra Ollama call per retain job**, over the tail of
    /// the transcript rather than the whole of it. That is the reason this is
    /// a knob and not a constant: it is a new GPU consumer on a path that
    /// already competes with extraction for the same single inference slot,
    /// and turning it off has to be one line rather than a rebuild.
    pub write_task_ledger: bool,
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
                detect_supersession: false,
                supersession_candidates: 12,
                write_task_ledger: true,
            },
            recall: RecallConfig {
                types: vec![FactType::World, FactType::Observation, FactType::Experience],
                limit: 20,
                max_tokens: 1024,
                // On, at the middle of the plateau the AX-2 sweep found:
                // 0.05..=0.15 all beat legacy scoring on every aggregate, so
                // the value is not a knife-edge argmax. The one term here
                // that is not legacy's — see
                // `recall::scoring::combined_with_semantic`.
                semantic_alpha: 0.1,
                cap_per_source: 0,
                preamble: String::new(),
            },
            reranker: RerankerConfig {
                enabled: false,
                model: DEFAULT_RERANK_MODEL.to_string(),
                top_k: 10,
                threads: 4,
                batch_size: 16,
            },
            mental: MentalConfig {
                refresh_interval_secs: 600,
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
            hooks: HooksConfig {
                enabled: true,
                mode: "shadow".to_string(),
                daemon_url: "http://127.0.0.1:9100".to_string(),
                connect_timeout_ms: 50,
                recall_timeout_ms: 400,
                retain_timeout_ms: 5000,
                // legacy: lib/config.py:34 retainEveryNTurns.
                retain_every_n_turns: 10,
                // legacy: lib/config.py:19 recallMaxQueryChars.
                recall_max_query_chars: 800,
                breaker_failures: 3,
                breaker_cooldown_secs: 60,
                max_reject_failures: 10,
                poison_retry_secs: 3600,
                catchup_max_sessions: 3,
                session_retention_days: 90,
                max_post_bytes: 24 * 1024 * 1024,
                max_inject_bytes: 64 * 1024,
                shadow_log_max_bytes: 64 * 1024 * 1024,
                state_dir: paths::hooks_state_dir()?,
                debug: false,
                bank_id: String::new(),
                // legacy: bank.py:26 DEFAULT_BANK_NAME / :124 agentName.
                agent_name: "claude-code".to_string(),
                directory_bank_map: HashMap::new(),
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
            ENV_HOOKS_DISABLE,
            ENV_DAEMON_URL,
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
            if let Some(v) = retain.detect_supersession {
                cfg.retain.detect_supersession = v;
            }
            if let Some(v) = retain.supersession_candidates {
                cfg.retain.supersession_candidates = v;
            }
            if let Some(v) = retain.write_task_ledger {
                cfg.retain.write_task_ledger = v;
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
            if let Some(v) = recall.semantic_alpha {
                cfg.recall.semantic_alpha = v;
            }
            if let Some(v) = recall.cap_per_source {
                cfg.recall.cap_per_source = v;
            }
            if let Some(v) = recall.preamble {
                cfg.recall.preamble = v;
            }
        }
        if let Some(r) = parsed.reranker {
            if let Some(v) = r.enabled {
                cfg.reranker.enabled = v;
            }
            if let Some(v) = r.model {
                cfg.reranker.model = v;
            }
            if let Some(v) = r.top_k {
                cfg.reranker.top_k = v;
            }
            if let Some(v) = r.threads {
                cfg.reranker.threads = v;
            }
            if let Some(v) = r.batch_size {
                cfg.reranker.batch_size = v;
            }
        }
        if let Some(v) = parsed.mental.and_then(|m| m.refresh_interval_secs) {
            cfg.mental.refresh_interval_secs = v;
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
        if let Some(hooks) = parsed.hooks {
            if let Some(v) = hooks.enabled {
                cfg.hooks.enabled = v;
            }
            if let Some(v) = hooks.mode {
                cfg.hooks.mode = v;
            }
            if let Some(v) = hooks.daemon_url {
                cfg.hooks.daemon_url = v;
            }
            if let Some(v) = hooks.connect_timeout_ms {
                cfg.hooks.connect_timeout_ms = v;
            }
            if let Some(v) = hooks.recall_timeout_ms {
                cfg.hooks.recall_timeout_ms = v;
            }
            if let Some(v) = hooks.retain_timeout_ms {
                cfg.hooks.retain_timeout_ms = v;
            }
            if let Some(v) = hooks.retain_every_n_turns {
                cfg.hooks.retain_every_n_turns = v;
            }
            if let Some(v) = hooks.recall_max_query_chars {
                cfg.hooks.recall_max_query_chars = v;
            }
            if let Some(v) = hooks.breaker_failures {
                cfg.hooks.breaker_failures = v;
            }
            if let Some(v) = hooks.breaker_cooldown_secs {
                cfg.hooks.breaker_cooldown_secs = v;
            }
            if let Some(v) = hooks.max_reject_failures {
                cfg.hooks.max_reject_failures = v;
            }
            if let Some(v) = hooks.poison_retry_secs {
                cfg.hooks.poison_retry_secs = v;
            }
            if let Some(v) = hooks.catchup_max_sessions {
                cfg.hooks.catchup_max_sessions = v;
            }
            if let Some(v) = hooks.session_retention_days {
                cfg.hooks.session_retention_days = v;
            }
            if let Some(v) = hooks.max_post_bytes {
                cfg.hooks.max_post_bytes = v;
            }
            if let Some(v) = hooks.max_inject_bytes {
                cfg.hooks.max_inject_bytes = v;
            }
            if let Some(v) = hooks.shadow_log_max_bytes {
                cfg.hooks.shadow_log_max_bytes = v;
            }
            if let Some(v) = hooks.state_dir {
                cfg.hooks.state_dir = expand_tilde(&v, home);
            }
            if let Some(v) = hooks.debug {
                cfg.hooks.debug = v;
            }
            if let Some(v) = hooks.bank_id {
                cfg.hooks.bank_id = v;
            }
            if let Some(v) = hooks.agent_name {
                cfg.hooks.agent_name = v;
            }
            if let Some(v) = hooks.directory_bank_map {
                cfg.hooks.directory_bank_map = v;
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
        cfg.retain.include_tool_calls = is_truthy(raw);
        explicit.push("include_tool_calls");
    }
    if let Some(name) = env.get(ENV_PROFILE) {
        cfg.profile.name = name.clone();
    }
    if let Some(raw) = env.get(ENV_HOOKS_DISABLE) {
        // Same truthy set as `_cast_env` (`lib/config.py:136`), and the same
        // set `memgarden_cli::hooks_disabled` applies before any config load —
        // the two must agree or the pre-config short-circuit would disagree
        // with what `hooks status` reports.
        if is_truthy(raw) {
            cfg.hooks.enabled = false;
        }
    }
    if let Some(url) = env.get(ENV_DAEMON_URL) {
        cfg.hooks.daemon_url = url.clone();
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
    // A zero here is not "off" — `enabled` is off. `top_k = 0` would rerank
    // nothing while still paying the model load, and (because the tail is
    // dropped) return an empty result set for every query; `threads` or
    // `batch_size` at 0 is rejected by ONNX Runtime / fastembed at the first
    // request instead of at boot, which is the wrong place to find out.
    for (name, value) in [
        ("reranker.top_k", cfg.reranker.top_k),
        ("reranker.threads", cfg.reranker.threads),
        ("reranker.batch_size", cfg.reranker.batch_size),
    ] {
        if value == 0 {
            return Err(Error::Config(format!("{name} must be > 0")));
        }
    }
    if cfg.reranker.top_k > MAX_RERANK_TOP_K {
        return Err(Error::Config(format!(
            "reranker.top_k must be <= {MAX_RERANK_TOP_K}: {}",
            cfg.reranker.top_k
        )));
    }
    if cfg.reranker.model.trim().is_empty() {
        return Err(Error::Config(
            "reranker.model must be a Hugging Face repo id".to_string(),
        ));
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
    // Upper bounds on the two knobs that reproduce the incident's wall-clock
    // term. `batch_size`'s LIMIT materialises every selected row into memory
    // and multiplies the per-round LLM call count; `max_attempts` multiplies
    // the per-batch wall clock directly (attempts x the client's 600s total
    // deadline). Neither has a legitimate value near these ceilings — they
    // exist so a typo cannot turn a 300s tick into an hour.
    if cfg.consolidation.batch_size > 500 {
        return Err(Error::Config(format!(
            "consolidation.batch_size must be <= 500: {}",
            cfg.consolidation.batch_size
        )));
    }
    if cfg.consolidation.max_attempts > 10 {
        return Err(Error::Config(format!(
            "consolidation.max_attempts must be <= 10: {}",
            cfg.consolidation.max_attempts
        )));
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

    if !matches!(cfg.hooks.mode.as_str(), "shadow" | "full") {
        return Err(Error::Config(format!(
            "hooks.mode must be shadow|full: {}",
            cfg.hooks.mode
        )));
    }
    // Same shape as the `[ollama]` check above, and for the same reason: a
    // typo'd URL would otherwise surface only as a silent fail-open on every
    // prompt, which is the failure mode hardest to notice.
    if !cfg.hooks.daemon_url.starts_with("http://") {
        return Err(Error::Config(format!(
            "hooks.daemon_url must start with http:// (loopback, no TLS): {}",
            cfg.hooks.daemon_url
        )));
    }
    // A zero timeout is not "no timeout" — it is a socket option that fails
    // every read immediately, i.e. a permanently broken hook that still
    // exits 0. Zero breaker_failures would open the breaker before the first
    // request and never make one.
    for (name, value) in [
        ("hooks.connect_timeout_ms", cfg.hooks.connect_timeout_ms),
        ("hooks.recall_timeout_ms", cfg.hooks.recall_timeout_ms),
        ("hooks.retain_timeout_ms", cfg.hooks.retain_timeout_ms),
        ("hooks.retain_every_n_turns", cfg.hooks.retain_every_n_turns),
        ("hooks.breaker_failures", cfg.hooks.breaker_failures as u64),
        (
            "hooks.max_reject_failures",
            cfg.hooks.max_reject_failures as u64,
        ),
        ("hooks.max_post_bytes", cfg.hooks.max_post_bytes as u64),
        ("hooks.max_inject_bytes", cfg.hooks.max_inject_bytes as u64),
        // A zero cooldown makes the breaker open and immediately close, i.e.
        // no breaker; a zero poison retry turns the hourly throttle back into
        // every-turn hammering; a zero shadow-log cap rotates on every write.
        // `catchup_max_sessions = 0` is deliberately NOT here — zero is its
        // documented "disable catch-up".
        (
            "hooks.breaker_cooldown_secs",
            cfg.hooks.breaker_cooldown_secs,
        ),
        ("hooks.poison_retry_secs", cfg.hooks.poison_retry_secs),
        ("hooks.shadow_log_max_bytes", cfg.hooks.shadow_log_max_bytes),
    ] {
        if value == 0 {
            return Err(Error::Config(format!("{name} must be > 0")));
        }
    }
    if cfg.hooks.max_post_bytes > DAEMON_MAX_BODY_BYTES {
        return Err(Error::Config(format!(
            "hooks.max_post_bytes must be <= {DAEMON_MAX_BODY_BYTES} (the daemon's \
             MAX_RETAIN_BODY_BYTES): {}",
            cfg.hooks.max_post_bytes
        )));
    }
    // Bounded above as well as below, because the consumer multiplies it:
    // `metrics_task::tick` computes `now_ms() - days * 86_400_000` as `i64`,
    // and a large enough value overflows to a cutoff in the *future* — a GC
    // that deletes every session row instead of none. 100 years is past any
    // legitimate retention and far below the overflow.
    if !(1..=36_500).contains(&cfg.hooks.session_retention_days) {
        return Err(Error::Config(format!(
            "hooks.session_retention_days must be 1..=36500: {}",
            cfg.hooks.session_retention_days
        )));
    }
    // Bounded above for the same shape of reason, one layer down: the value is
    // a **character** count and the daemon's limit is a **byte** count. A char
    // is at most 4 UTF-8 bytes, so anything over `MAX_QUERY_BYTES / 4` can
    // produce a query the daemon 400s — on every prompt, from a config that
    // looks fine. Below it, a 400 for length is unreachable by construction.
    if cfg.hooks.max_inject_bytes > MAX_INJECT_BYTES_CEILING {
        return Err(Error::Config(format!(
            "hooks.max_inject_bytes must be <= {MAX_INJECT_BYTES_CEILING} (the hook client's \
             response-body cap; JSON escaping inflates what reaches the wire by up to 6x): {}",
            cfg.hooks.max_inject_bytes
        )));
    }
    if !(1..=MAX_RECALL_QUERY_CHARS).contains(&cfg.hooks.recall_max_query_chars) {
        return Err(Error::Config(format!(
            "hooks.recall_max_query_chars must be 1..={MAX_RECALL_QUERY_CHARS}: {}",
            cfg.hooks.recall_max_query_chars
        )));
    }

    Ok(cfg)
}

/// The fork's `_cast_env` truthy set (`lib/config.py:136`).
fn is_truthy(raw: &str) -> bool {
    matches!(raw.to_ascii_lowercase().as_str(), "true" | "1" | "yes")
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
    reranker: Option<TomlReranker>,
    consolidation: Option<TomlConsolidation>,
    mental: Option<TomlMental>,
    profile: Option<TomlProfile>,
    hooks: Option<TomlHooks>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlHooks {
    enabled: Option<bool>,
    mode: Option<String>,
    daemon_url: Option<String>,
    connect_timeout_ms: Option<u64>,
    recall_timeout_ms: Option<u64>,
    retain_timeout_ms: Option<u64>,
    retain_every_n_turns: Option<u64>,
    recall_max_query_chars: Option<usize>,
    breaker_failures: Option<u32>,
    breaker_cooldown_secs: Option<u64>,
    max_reject_failures: Option<u32>,
    poison_retry_secs: Option<u64>,
    catchup_max_sessions: Option<usize>,
    session_retention_days: Option<u64>,
    max_post_bytes: Option<usize>,
    max_inject_bytes: Option<usize>,
    shadow_log_max_bytes: Option<u64>,
    state_dir: Option<String>,
    debug: Option<bool>,
    bank_id: Option<String>,
    agent_name: Option<String>,
    directory_bank_map: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlReranker {
    enabled: Option<bool>,
    model: Option<String>,
    top_k: Option<usize>,
    threads: Option<usize>,
    batch_size: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlMental {
    refresh_interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    detect_supersession: Option<bool>,
    supersession_candidates: Option<usize>,
    write_task_ledger: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct TomlRecall {
    types: Option<Vec<FactType>>,
    limit: Option<usize>,
    max_tokens: Option<usize>,
    semantic_alpha: Option<f64>,
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
                detect_supersession: false,
                supersession_candidates: 12,
                write_task_ledger: true,
            },
            recall: RecallConfig {
                types: vec![FactType::World, FactType::Observation, FactType::Experience],
                limit: 20,
                max_tokens: 1024,
                semantic_alpha: 0.0,
                cap_per_source: 0,
                preamble: String::new(),
            },
            reranker: RerankerConfig {
                enabled: false,
                model: DEFAULT_RERANK_MODEL.to_string(),
                top_k: 10,
                threads: 4,
                batch_size: 16,
            },
            mental: MentalConfig {
                refresh_interval_secs: 600,
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
            hooks: HooksConfig {
                enabled: true,
                mode: "shadow".to_string(),
                daemon_url: "http://127.0.0.1:9100".to_string(),
                connect_timeout_ms: 50,
                recall_timeout_ms: 400,
                retain_timeout_ms: 5000,
                retain_every_n_turns: 10,
                recall_max_query_chars: 800,
                breaker_failures: 3,
                breaker_cooldown_secs: 60,
                max_reject_failures: 10,
                poison_retry_secs: 3600,
                catchup_max_sessions: 3,
                session_retention_days: 90,
                max_post_bytes: 24 * 1024 * 1024,
                max_inject_bytes: 64 * 1024,
                shadow_log_max_bytes: 64 * 1024 * 1024,
                state_dir: PathBuf::from("/data/hooks"),
                debug: false,
                bank_id: String::new(),
                agent_name: "claude-code".to_string(),
                directory_bank_map: HashMap::new(),
            },
        }
    }

    /// The struct defaults are the ones that ship, so they are asserted
    /// against the plan's table rather than left to whatever the literal says.
    #[test]
    fn hooks_defaults_are_the_planned_ones() {
        let cfg = from_parts(defaults(), None, &HashMap::new()).unwrap();
        let h = &cfg.hooks;
        assert!(h.enabled);
        // Shadow injects nothing. If this ever defaults to "full", installing
        // the switch throws it — plan §Binding decisions #13.
        assert_eq!(h.mode, "shadow");
        assert_eq!(h.daemon_url, "http://127.0.0.1:9100");
        assert_eq!(h.connect_timeout_ms, 50);
        assert_eq!(h.recall_timeout_ms, 400);
        assert_eq!(h.retain_timeout_ms, 5000);
        assert_eq!(h.retain_every_n_turns, 10, "legacy lib/config.py:34");
        assert_eq!(h.breaker_failures, 3);
        assert_eq!(h.breaker_cooldown_secs, 60);
        assert_eq!(h.max_reject_failures, 10);
        assert_eq!(h.poison_retry_secs, 3600);
        assert_eq!(h.catchup_max_sessions, 3);
        assert_eq!(h.session_retention_days, 90);
        assert_eq!(h.max_post_bytes, 24 * 1024 * 1024);
        assert_eq!(h.max_inject_bytes, 64 * 1024);
        assert_eq!(h.shadow_log_max_bytes, 64 * 1024 * 1024);
        assert!(!h.debug);
        assert_eq!(h.bank_id, "");
        assert_eq!(h.agent_name, "claude-code");
        assert!(h.directory_bank_map.is_empty());
    }

    #[test]
    fn hooks_toml_and_env_precedence() {
        let toml_str = r#"
            [hooks]
            mode = "full"
            daemon_url = "http://127.0.0.1:9199"
            recall_timeout_ms = 250
            state_dir = "~/hookstate"
            debug = true
            bank_id = "pinned"
            agent_name = "codex"
            [hooks.directory_bank_map]
            "/srv/one" = "bank-one"
        "#;
        let mut env = HashMap::new();
        env.insert(ENV_HOME.to_string(), "/home/u".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(cfg.hooks.mode, "full");
        assert_eq!(cfg.hooks.daemon_url, "http://127.0.0.1:9199");
        assert_eq!(cfg.hooks.recall_timeout_ms, 250);
        assert_eq!(cfg.hooks.state_dir, PathBuf::from("/home/u/hookstate"));
        assert!(cfg.hooks.debug);
        assert_eq!(cfg.hooks.bank_id, "pinned");
        assert_eq!(cfg.hooks.agent_name, "codex");
        assert_eq!(
            cfg.hooks
                .directory_bank_map
                .get("/srv/one")
                .map(String::as_str),
            Some("bank-one")
        );
        // Untouched by this TOML: still default.
        assert_eq!(cfg.hooks.retain_timeout_ms, 5000);

        // Env beats TOML, for both hook env keys.
        env.insert(
            ENV_DAEMON_URL.to_string(),
            "http://localhost:9198".to_string(),
        );
        env.insert(ENV_HOOKS_DISABLE.to_string(), "1".to_string());
        let cfg = from_parts(defaults(), Some(toml_str), &env).unwrap();
        assert_eq!(cfg.hooks.daemon_url, "http://localhost:9198");
        assert!(!cfg.hooks.enabled);

        // Only the truthy set disables. "0" is not a disable, and reading it
        // as one would silently turn the hooks off for anyone who wrote
        // `MEMGARDEN_HOOKS_DISABLE=0` meaning "on".
        env.insert(ENV_HOOKS_DISABLE.to_string(), "0".to_string());
        assert!(
            from_parts(defaults(), Some(toml_str), &env)
                .unwrap()
                .hooks
                .enabled
        );
    }

    #[test]
    fn hooks_validation_rejects_unusable_values() {
        let cases = [
            ("[hooks]\nmode = \"loud\"", "mode"),
            ("[hooks]\ndaemon_url = \"https://127.0.0.1:9100\"", "https"),
            ("[hooks]\nconnect_timeout_ms = 0", "connect"),
            ("[hooks]\nrecall_timeout_ms = 0", "recall"),
            ("[hooks]\nretain_timeout_ms = 0", "retain"),
            ("[hooks]\nretain_every_n_turns = 0", "turn gate"),
            ("[hooks]\nbreaker_failures = 0", "breaker"),
            ("[hooks]\nmax_reject_failures = 0", "reject"),
            ("[hooks]\nmax_post_bytes = 0", "post bytes"),
            ("[hooks]\nmax_inject_bytes = 0", "inject bytes"),
            ("[hooks]\nsession_retention_days = 0", "retention"),
            // `now_ms() - days * 86_400_000` as i64 overflows to a cutoff in
            // the *future*, and the GC then deletes every session row.
            (
                "[hooks]\nsession_retention_days = 200000000000000",
                "retention overflow",
            ),
            ("[hooks]\nbreaker_cooldown_secs = 0", "breaker cooldown"),
            ("[hooks]\npoison_retry_secs = 0", "poison retry"),
            ("[hooks]\nshadow_log_max_bytes = 0", "shadow log"),
            // Above the daemon's own body limit this could only ever 413.
            (
                "[hooks]\nmax_post_bytes = 33554433",
                "over the daemon limit",
            ),
            ("[hooks]\nrecall_max_query_chars = 0", "empty query"),
            // 8 MB + 1 of raw injection is ~48 MB on the wire after escaping,
            // and cannot arrive anyway: the client's response cap is 8 MB.
            (
                "[hooks]\nmax_inject_bytes = 8388609",
                "over the response cap",
            ),
            // 2049 chars of 4-byte UTF-8 is 8196 bytes, past the daemon's
            // 8 KB MAX_QUERY_BYTES — a 400 on every prompt. The unit
            // conversion is the whole reason this bound is not just "big".
            (
                "[hooks]\nrecall_max_query_chars = 2049",
                "over the daemon query limit",
            ),
        ];
        for (toml_str, what) in cases {
            assert!(
                from_parts(defaults(), Some(toml_str), &HashMap::new()).is_err(),
                "expected {what} to be rejected: {toml_str}"
            );
        }
        // …and the boundaries themselves are allowed.
        for ok in [
            "[hooks]\nmax_post_bytes = 33554432",
            "[hooks]\nsession_retention_days = 36500",
            "[hooks]\nrecall_max_query_chars = 2048",
            "[hooks]\nmax_inject_bytes = 8388608",
            // 0 is this knob's documented "disable catch-up", not an error.
            "[hooks]\ncatchup_max_sessions = 0",
        ] {
            assert!(
                from_parts(defaults(), Some(ok), &HashMap::new()).is_ok(),
                "{ok}"
            );
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

    /// The default that carries the parity claim. If this flips, recall stops
    /// matching the live legacy daemon (`RERANKER_PROVIDER=rrf`) and starts
    /// spending 1.5–2.6ms per candidate against a 35ms p50 budget — so it
    /// gets its own assertion rather than riding along in a bigger test.
    #[test]
    fn reranker_defaults_are_off_and_shallow() {
        let cfg = from_parts(defaults(), None, &HashMap::new()).unwrap();
        assert!(
            !cfg.reranker.enabled,
            "off by default IS parity with the live legacy daemon"
        );
        assert_eq!(cfg.reranker.model, "Xenova/ms-marco-MiniLM-L-6-v2");
        assert_eq!(cfg.reranker.top_k, 10, "NOT legacy's thinking_budget*2=600");
        assert_eq!(cfg.reranker.threads, 4);
        assert_eq!(cfg.reranker.batch_size, 16);

        let toml_str = r#"
            [reranker]
            enabled = true
            top_k = 5
            threads = 7
            batch_size = 3
        "#;
        let cfg = from_parts(defaults(), Some(toml_str), &HashMap::new()).unwrap();
        assert!(cfg.reranker.enabled);
        assert_eq!(cfg.reranker.top_k, 5);
        assert_eq!(cfg.reranker.threads, 7);
        assert_eq!(cfg.reranker.batch_size, 3);
        // Untouched by TOML: still default.
        assert_eq!(cfg.reranker.model, "Xenova/ms-marco-MiniLM-L-6-v2");
    }

    #[test]
    fn reranker_rejects_zero_and_oversized_knobs() {
        let env = HashMap::new();
        for key in ["top_k", "threads", "batch_size"] {
            let toml_str = format!("[reranker]\n{key} = 0\n");
            assert!(
                from_parts(defaults(), Some(&toml_str), &env).is_err(),
                "reranker.{key} = 0 must be rejected"
            );
        }
        let over = format!("[reranker]\ntop_k = {}\n", MAX_RERANK_TOP_K + 1);
        assert!(from_parts(defaults(), Some(&over), &env).is_err());
        let at = format!("[reranker]\ntop_k = {MAX_RERANK_TOP_K}\n");
        assert!(from_parts(defaults(), Some(&at), &env).is_ok());
        assert!(
            from_parts(defaults(), Some("[reranker]\nmodel = \"  \"\n"), &env).is_err(),
            "a blank repo id would fail at load, not at boot"
        );
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
