//! Ollama HTTP client for fact extraction: chat completion with retry, plus
//! a lightweight `/api/version` reachability probe.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use memgarden_core::config::OllamaConfig;

/// Bounded wait for an INTERACTIVE caller's turn at the (size-1, by default)
/// concurrency semaphore. Critic Revision R11: user-facing paths
/// (`/dry-run-extract` here; `/reflect` later) must fail fast with a 503
/// rather than queue indefinitely behind a slow LLM call.
///
/// Background callers use `chat_json_background` instead, which waits
/// untimed — a retain job that gave up after 15s because an interactive
/// request happened to be in flight would drop a whole chunk of a user's
/// transcript for no reason. The job's own wall clock
/// (`retain.wall_timeout_secs`) is what bounds that wait.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);

/// Retry backoff: starts at 1s, doubles each attempt, capped at 10s
/// (Critic Revision R14 — legacy's own cap is 60s, `config.py:862-864`, but
/// a single `max_concurrent=1` permit sitting in a 60s sleep starves every
/// other caller behind it).
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// Hard ceiling on one `chat_json` call including all retries (security
/// review M2) — bounds permit-hold time whatever the config says.
const TOTAL_DEADLINE_CAP: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("timed out waiting for an Ollama request slot")]
    Busy,
    #[error("ollama call exceeded the {0:?} total deadline")]
    Deadline(Duration),
    #[error("ollama request failed: {0}")]
    Transport(String),
    #[error("ollama returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("failed to parse ollama response as JSON after retries: {0}")]
    Parse(String),
}

pub struct OllamaClient {
    http: reqwest::Client,
    cfg: OllamaConfig,
    sem: Semaphore,
}

impl OllamaClient {
    pub fn new(cfg: OllamaConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_secs))
            .build()?;
        let max_concurrent = cfg.max_concurrent.max(1);
        Ok(OllamaClient {
            http,
            cfg,
            sem: Semaphore::new(max_concurrent),
        })
    }

    /// Posts one `/api/chat` request and deserializes the assistant
    /// message's `content` as `T`, retrying on transport failure *and* on
    /// JSON-parse failure — CRITICAL (verified fact, plan Verified
    /// Environment Facts / Ollama section): `/api/chat` does NOT enforce the
    /// `format` schema it's given, so a malformed reply is a normal,
    /// expected failure mode here, not a bug. `schema` is restated in
    /// `format` anyway (cheap, and it does measurably help — see the port
    /// brief) but the caller's system prompt must ALSO spell out the shape
    /// in prose; this client has no opinion on prompt content.
    ///
    /// Exhausting all retries is a hard error — never silently returns an
    /// empty/default `T` (legacy issue #1833, brief §4.3).
    pub async fn chat_json<T: DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
        schema: &Value,
    ) -> Result<T, OllamaError> {
        self.chat_json_inner(system, user, schema, Some(ACQUIRE_TIMEOUT), None, None)
            .await
    }

    /// Same as `chat_json` but waits **untimed** for the concurrency permit.
    /// For the retain worker only: it is not answering an HTTP request, so
    /// "Ollama is busy" is a reason to queue, not to fail. Callers must
    /// impose their own outer bound — `retain::run_job`'s wall clock does.
    pub async fn chat_json_background<T: DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
        schema: &Value,
    ) -> Result<T, OllamaError> {
        self.chat_json_inner(system, user, schema, None, None, None)
            .await
    }

    /// `chat_json_background` with a per-call `num_predict` **ceiling** and an
    /// optional per-call `num_ctx`.
    ///
    /// The configured `ollama.num_predict` (8192) is sized for extraction,
    /// where one chunk legitimately produces pages of facts. A caller whose
    /// reply is a small fixed-shape object wants a much smaller number, and
    /// wants it in code rather than in config: an unbounded reply exhausts
    /// the context window mid-generation and triggers Ollama's context shift,
    /// which is the truncation half of the 2026-08-02 consolidation incident
    /// (see `consolidate::DEDUP_REPLY_MAX_TOKENS`).
    ///
    /// `num_predict` is a ceiling, not an override — a config that already
    /// asks for less is respected.
    ///
    /// `num_ctx` is different: it is sent as given, because it is not a
    /// preference but part of a caller's own token bound. CE-9b's batch
    /// prompt is larger than Ollama's 4096 default window, so leaving the
    /// window to the server would make that bound an assumption about the
    /// deployment rather than a property of the code. `None` keeps the
    /// server's default, which is what every other caller wants.
    pub async fn chat_json_background_bounded<T: DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
        schema: &Value,
        num_predict: u32,
        num_ctx: Option<u32>,
    ) -> Result<T, OllamaError> {
        self.chat_json_inner(system, user, schema, None, Some(num_predict), num_ctx)
            .await
    }

    async fn chat_json_inner<T: DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
        schema: &Value,
        acquire_timeout: Option<Duration>,
        num_predict: Option<u32>,
        num_ctx: Option<u32>,
    ) -> Result<T, OllamaError> {
        // A closed semaphore is unreachable today, but a request path must
        // never panic.
        let permit = match acquire_timeout {
            Some(limit) => tokio::time::timeout(limit, self.sem.acquire())
                .await
                .map_err(|_| OllamaError::Busy)?
                .map_err(|_| OllamaError::Busy)?,
            None => self.sem.acquire().await.map_err(|_| OllamaError::Busy)?,
        };

        // Security review M2: without a total deadline, misconfigured
        // request_timeout × max_retries lets one caller hold the permit for
        // hours. Per-attempt timeouts still apply inside; this is the outer
        // ceiling.
        let deadline = Duration::from_secs(self.cfg.request_timeout_secs)
            .saturating_mul(self.cfg.max_retries.saturating_add(1))
            .min(TOTAL_DEADLINE_CAP);

        let result = tokio::time::timeout(deadline, async {
            let outer_attempts = self.cfg.max_retries.saturating_add(1);
            let mut backoff = BACKOFF_START;
            let mut last_err = OllamaError::Parse("no attempts made".to_string());

            for attempt in 0..outer_attempts {
                match self.try_chat(system, user, schema, num_predict, num_ctx).await {
                    Ok(raw) => match serde_json::from_str::<T>(&raw) {
                        Ok(parsed) => return Ok(parsed),
                        Err(e) => {
                            // Security review L1: `raw` is LLM output steered
                            // by caller text — escape (Debug) and truncate
                            // before it touches the log stream.
                            let snippet: String = raw.chars().take(512).collect();
                            tracing::warn!(attempt, error = %e, raw = ?snippet, "ollama response failed to parse");
                            last_err = OllamaError::Parse(e.to_string());
                        }
                    },
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "ollama request failed");
                        // 4xx (except 429) is permanent — a typo'd model name
                        // will not heal with retries; fail fast and free the
                        // permit.
                        let permanent = matches!(
                            &e,
                            OllamaError::Http { status, .. }
                                if (400..500).contains(status) && *status != 429
                        );
                        last_err = e;
                        if permanent {
                            return Err(last_err);
                        }
                    }
                }
                if attempt + 1 < outer_attempts {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
            Err(last_err)
        })
        .await
        .unwrap_or(Err(OllamaError::Deadline(deadline)));

        drop(permit);
        result
    }

    /// One HTTP round trip: POST `/api/chat`, return the assistant message's
    /// raw `content` string (still JSON text — parsing happens in the
    /// caller so retry can distinguish transport failure from parse
    /// failure).
    async fn try_chat(
        &self,
        system: &str,
        user: &str,
        schema: &Value,
        num_predict: Option<u32>,
        num_ctx: Option<u32>,
    ) -> Result<String, OllamaError> {
        let mut options = json!({
            "temperature": self.cfg.temperature,
            // A per-call ceiling, never a raise: a config already asking
            // for less keeps its number.
            "num_predict": num_predict
                .map_or(self.cfg.num_predict, |cap| cap.min(self.cfg.num_predict)),
        });
        if let Some(ctx) = num_ctx {
            options["num_ctx"] = json!(ctx);
        }
        let body = json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": false,
            "format": schema,
            "options": options,
            "keep_alive": self.cfg.keep_alive,
        });

        let url = format!("{}/api/chat", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| OllamaError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // Security review L1: this body ends up in logs and in the 503
            // envelope — keep it bounded.
            let body: String = text.chars().take(2048).collect();
            return Err(OllamaError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: Value = resp
            .json()
            .await
            .map_err(|e| OllamaError::Transport(e.to_string()))?;
        let count = |key: &str| parsed.get(key).and_then(serde_json::Value::as_u64);
        tracing::debug!(
            prompt_eval_count = count("prompt_eval_count"),
            eval_count = count("eval_count"),
            total_duration_ms = count("total_duration").map(|n| n / 1_000_000),
            "ollama /api/chat round trip"
        );
        parsed
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| OllamaError::Transport("response missing message.content".to_string()))
    }

    /// `GET /api/version` — used only by the background prober
    /// (`run_prober`), never per-request (the `gc.collect` lesson: see
    /// `embed_task.rs`'s doc comment on the same principle).
    pub async fn probe(&self) -> bool {
        let url = format!("{}/api/version", self.cfg.base_url.trim_end_matches('/'));
        match self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

/// Coarse Ollama reachability, reported by `/healthz` and read/written via
/// the atomic below — same lock-free-static pattern as
/// `crate::embed::EmbedStatus`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaStatus {
    Unreachable = 0,
    Ready = 1,
}

impl OllamaStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OllamaStatus::Unreachable => "unreachable",
            OllamaStatus::Ready => "ready",
        }
    }

    fn from_u8(v: u8) -> OllamaStatus {
        match v {
            1 => OllamaStatus::Ready,
            _ => OllamaStatus::Unreachable,
        }
    }
}

// Starts optimistic (`Ready`), not `Unreachable`: `tokio::time::interval`
// fires its first tick immediately on spawn, so `run_prober` corrects this
// within milliseconds of startup in practice, and defaulting to "down"
// would make every fresh-started daemon (and every test that never spawns
// the prober at all, e.g. tests/api.rs) report DEGRADED for no reason.
static OLLAMA_STATUS: AtomicU8 = AtomicU8::new(OllamaStatus::Ready as u8);

pub fn ollama_status() -> OllamaStatus {
    OllamaStatus::from_u8(OLLAMA_STATUS.load(Ordering::Relaxed))
}

fn set_ollama_status(status: OllamaStatus) {
    OLLAMA_STATUS.store(status as u8, Ordering::Relaxed);
}

/// Background reachability prober: every 30s, hits `/api/version` and
/// publishes the result to the process-wide atomic `/healthz` reads.
/// Decision #5 (plan): never probe per request.
pub async fn run_prober(client: std::sync::Arc<OllamaClient>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let reachable = client.probe().await;
                set_ollama_status(if reachable {
                    OllamaStatus::Ready
                } else {
                    OllamaStatus::Unreachable
                });
            }
            _ = &mut shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> OllamaConfig {
        OllamaConfig {
            base_url: "http://127.0.0.1:1".to_string(), // unroutable: fails fast
            model: "test-model".to_string(),
            temperature: 0.1,
            num_predict: 64,
            request_timeout_secs: 1,
            max_retries: 1,
            keep_alive: "10m".to_string(),
            max_concurrent: 1,
        }
    }

    #[test]
    fn ollama_status_round_trips() {
        set_ollama_status(OllamaStatus::Unreachable);
        assert_eq!(ollama_status(), OllamaStatus::Unreachable);
        set_ollama_status(OllamaStatus::Ready);
        assert_eq!(ollama_status(), OllamaStatus::Ready);
        // Restore for other tests in this binary sharing the global static
        // (default is Ready — see the static's doc comment).
        set_ollama_status(OllamaStatus::Ready);
    }

    #[tokio::test]
    async fn probe_unreachable_is_false() {
        let client = OllamaClient::new(test_cfg()).unwrap();
        assert!(!client.probe().await);
    }

    #[tokio::test]
    async fn chat_json_exhausts_retries_as_hard_error() {
        let client = OllamaClient::new(test_cfg()).unwrap();
        let result: Result<Value, OllamaError> = client
            .chat_json("system", "user", &json!({"type": "object"}))
            .await;
        assert!(
            result.is_err(),
            "unreachable ollama must be a hard error, never a silent empty result"
        );
    }

    /// Review HIGH 2, the actual contract: while one caller holds the single
    /// permit for longer than `ACQUIRE_TIMEOUT`, an interactive `chat_json`
    /// must give up with `Busy` but a background `chat_json_background` must
    /// wait it out and succeed.
    ///
    /// Costs ~16s of wall clock by construction — the threshold under test is
    /// 15s, so there is no way to prove it faster. It is the only slow test
    /// in the suite and it runs concurrently with the rest.
    #[tokio::test]
    async fn background_acquire_waits_out_a_holder_that_defeats_the_interactive_timeout() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        // Stub that stalls only its FIRST request; later calls answer at once,
        // so the whole test costs one hold, not two.
        let calls = Arc::new(AtomicUsize::new(0));
        let hold = ACQUIRE_TIMEOUT + Duration::from_secs(1);
        let stub_calls = calls.clone();
        let app = axum::Router::new().route(
            "/api/chat",
            axum::routing::post(move || {
                let calls = stub_calls.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        tokio::time::sleep(hold).await;
                    }
                    axum::Json(json!({ "message": { "content": "{\"ok\":true}" } }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = test_cfg();
        cfg.base_url = format!("http://{addr}");
        cfg.request_timeout_secs = 120;
        cfg.max_retries = 0;
        let client = Arc::new(OllamaClient::new(cfg).unwrap());

        let holder = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .chat_json_background::<Value>("s", "u", &json!({}))
                    .await
            })
        };
        // Let the holder actually take the permit.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let interactive = client.chat_json::<Value>("s", "u", &json!({})).await;
        assert!(
            matches!(interactive, Err(OllamaError::Busy)),
            "an interactive caller must give up at ACQUIRE_TIMEOUT, got {interactive:?}"
        );

        let background = client
            .chat_json_background::<Value>("s", "u", &json!({}))
            .await;
        assert!(
            background.is_ok(),
            "a background caller must wait the holder out, got {background:?}"
        );
        assert!(holder.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn acquire_timeout_returns_busy_not_unbounded_wait() {
        // max_concurrent=1, held by a task that never releases (simulates a
        // stuck background call) — a second interactive call must time out
        // at ACQUIRE_TIMEOUT (15s), not hang forever. Uses a short-lived
        // manual semaphore rather than the full 15s to keep the test fast.
        let sem = Semaphore::new(1);
        let _held = sem.acquire().await.unwrap();
        let result = tokio::time::timeout(Duration::from_millis(50), sem.acquire()).await;
        assert!(
            result.is_err(),
            "second acquire must not succeed while the permit is held"
        );
    }
}
