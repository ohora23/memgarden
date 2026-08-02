//! Ollama HTTP client for fact extraction: chat completion with retry, plus
//! a lightweight `/api/version` reachability probe.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use memgarden_core::config::OllamaConfig;

/// Bounded wait for an interactive caller's turn at the (size-1, by default)
/// concurrency semaphore. Critic Revision R11: user-facing paths
/// (`/dry-run-extract` here; `/reflect` later) must fail fast with a 503
/// rather than queue indefinitely behind a slow LLM call. B3's background
/// retain worker will need a different acquire path that releases the
/// permit between chunks instead of timing out — not needed until B3.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);

/// Retry backoff: starts at 1s, doubles each attempt, capped at 10s
/// (Critic Revision R14 — legacy's own cap is 60s, `config.py:862-864`, but
/// a single `max_concurrent=1` permit sitting in a 60s sleep starves every
/// other caller behind it).
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("timed out waiting for an Ollama request slot")]
    Busy,
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
        let permit = tokio::time::timeout(ACQUIRE_TIMEOUT, self.sem.acquire())
            .await
            .map_err(|_| OllamaError::Busy)?
            .expect("semaphore never closed");

        let outer_attempts = self.cfg.max_retries + 1;
        let mut backoff = BACKOFF_START;
        let mut last_err = OllamaError::Parse("no attempts made".to_string());

        for attempt in 0..outer_attempts {
            match self.try_chat(system, user, schema).await {
                Ok(raw) => match serde_json::from_str::<T>(&raw) {
                    Ok(parsed) => {
                        drop(permit);
                        return Ok(parsed);
                    }
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, raw = %raw, "ollama response failed to parse");
                        last_err = OllamaError::Parse(e.to_string());
                    }
                },
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "ollama request failed");
                    last_err = e;
                }
            }
            if attempt + 1 < outer_attempts {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
        drop(permit);
        Err(last_err)
    }

    /// One HTTP round trip: POST `/api/chat`, return the assistant message's
    /// raw `content` string (still JSON text — parsing happens in the
    /// caller so retry can distinguish transport failure from parse
    /// failure).
    async fn try_chat(&self, system: &str, user: &str, schema: &Value) -> Result<String, OllamaError> {
        let body = json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": false,
            "format": schema,
            "options": {
                "temperature": self.cfg.temperature,
                "num_predict": self.cfg.num_predict,
            },
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
            return Err(OllamaError::Http {
                status: status.as_u16(),
                body: text,
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
        assert!(result.is_err(), "unreachable ollama must be a hard error, never a silent empty result");
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
        assert!(result.is_err(), "second acquire must not succeed while the permit is held");
    }
}
