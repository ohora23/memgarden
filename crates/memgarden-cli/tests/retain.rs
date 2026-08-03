//! `hook retain` and `hook session-end` against a real socket and the real
//! binary.
//!
//! The subject is a **process**: an exit code, an empty stdout, the *absence*
//! of a request, and a state file written by a child that has already been
//! reaped are none of them observable from a `dispatch()` call.
//!
//! The stub counts **accepts**, not requests — the difference between "the
//! turn gate skipped the work" and "the turn gate skipped the socket". The
//! plan asks for the second everywhere it asks for a gate.
//!
//! Every listener binds port **0**. 9077 (legacy hindsight) and 9090 (memdash)
//! are live on this machine and are never touched (plan §Cross-PR rules 1).
//! `MEMGARDEN_CONFIG`, `HOME` and `XDG_DATA_HOME` are redirected into a temp
//! dir, and `state_dir` is pinned explicitly on top of that.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- the stub

struct Stub {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    accepts: Arc<AtomicUsize>,
}

impl Stub {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
    /// The request lines (`METHOD path`) in order — the cheapest way to say
    /// "these three requests, in this order, and no fourth".
    fn lines(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| {
                let mut parts = r.split(' ');
                format!(
                    "{} {}",
                    parts.next().unwrap_or(""),
                    parts.next().unwrap_or("")
                )
            })
            .collect()
    }
    fn body(&self, n: usize) -> serde_json::Value {
        let raw = self.requests()[n].clone();
        let (_, body) = raw.split_once("\r\n\r\n").expect("a body");
        serde_json::from_str(body).expect("valid json body")
    }
    /// Every retain body, in order. The retain path is the only one that
    /// carries `messages`, so this is a filter rather than an index.
    fn retains(&self) -> Vec<serde_json::Value> {
        (0..self.requests().len())
            .map(|n| self.body(n))
            .filter(|b| b.get("messages").is_some())
            .collect()
    }
}

fn stub(reply: impl Fn(usize, &str) -> String + Send + 'static) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let accepts = Arc::new(AtomicUsize::new(0));
    let recorder = requests.clone();
    let counter = accepts.clone();
    std::thread::spawn(move || {
        for (n, sock) in listener.incoming().enumerate() {
            let Ok(mut sock) = sock else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request(&mut sock);
            let response = reply(n, &request);
            recorder.lock().unwrap().push(request);
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    });
    Stub {
        url,
        requests,
        accepts,
    }
}

fn read_request(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let length: usize = head
                .split("\r\n")
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + length {
                break;
            }
        }
        match sock.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// The daemon's identity token. Every stub reply carries it, because
/// `parse_head` refuses a response that cannot produce it — before a body byte
/// is read (C3). A stub that forgot it would make every test here read as a
/// transport failure.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn json_reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nx-memgarden-token: {TOKEN}\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn accepted(job_id: &str) -> String {
    json_reply(
        "202 Accepted",
        &serde_json::json!({
            "status": "accepted", "job_id": job_id, "document_id": 7,
            "raw_tokens": 900, "capped_tokens": 300,
            "saved_tokens": 600, "saving_ratio": 0.667,
        })
        .to_string(),
    )
}

fn settled(status: &str) -> String {
    json_reply(
        "200 OK",
        &serde_json::json!({
            "status": status, "job_id": null, "document_id": 7,
            "raw_tokens": 0, "capped_tokens": 0,
            "saved_tokens": 0, "saving_ratio": 0.0,
        })
        .to_string(),
    )
}

fn job(status: &str) -> String {
    json_reply(
        "200 OK",
        &serde_json::json!({
            "job_id": "j1", "bank_id": "b", "document_id": 7, "session_id": "s1",
            "status": status, "chunks_total": 4, "chunks_done": 4,
            "chunks_skipped": 0, "chunks_failed": 0, "facts_written": 12,
            "error": null, "detail": null, "created_at": 1, "updated_at": 2,
        })
        .to_string(),
    )
}

fn error(status: &str) -> String {
    json_reply(status, &serde_json::json!({"error": "nope"}).to_string())
}

/// Which endpoint a request line names, so a stub can answer three routes
/// without re-parsing them each time.
fn route(request: &str) -> &'static str {
    let line = request.lines().next().unwrap_or("");
    if line.contains("/retain") && line.starts_with("POST") {
        "retain"
    } else if line.starts_with("GET /v1/retain/") {
        "job"
    } else if line.contains("/sessions") {
        "sessions"
    } else {
        "banks"
    }
}

// ------------------------------------------------------------- the fixture

struct Fixture {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    state_dir: PathBuf,
    project: PathBuf,
    home: PathBuf,
    transcript: PathBuf,
}

fn fixture(extra: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let project = tmp.path().join("demo-project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let data = home.join("data").join("memgarden");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("daemon.token"), TOKEN).unwrap();
    let config = tmp.path().join("memgarden.toml");
    std::fs::write(
        &config,
        format!(
            "[hooks]\nstate_dir = {:?}\n{extra}\n",
            state_dir.to_string_lossy()
        ),
    )
    .unwrap();
    let transcript = tmp.path().join("transcript.jsonl");
    Fixture {
        _tmp: tmp,
        config,
        state_dir,
        project,
        home,
        transcript,
    }
}

impl Fixture {
    fn run(&self, args: &[&str], stdin: &[u8], daemon_url: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_memgarden"))
            .args(args)
            .env("MEMGARDEN_CONFIG", &self.config)
            .env("MEMGARDEN_DAEMON_URL", daemon_url)
            .env("CLAUDE_PROJECT_DIR", &self.project)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env_remove("MEMGARDEN_HOOKS_DISABLE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn memgarden");
        let _ = child.stdin.take().expect("stdin").write_all(stdin);
        child.wait_with_output().expect("wait")
    }

    /// One `Stop`, with the payload Claude Code sends.
    fn stop(&self, session_id: &str, url: &str) -> Output {
        let out = self.run(&["hook", "retain"], &self.payload(session_id), url);
        assert_silent(&out, "retain");
        out
    }

    fn payload(&self, session_id: &str) -> Vec<u8> {
        serde_json::json!({
            "session_id": session_id,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": "/repo/sub",
            "hook_event_name": "Stop",
        })
        .to_string()
        .into_bytes()
    }

    fn state(&self, session_id: &str) -> serde_json::Value {
        let raw = std::fs::read(self.state_dir.join(format!("{session_id}.json")))
            .unwrap_or_else(|e| panic!("no state file for {session_id}: {e}"));
        serde_json::from_slice(&raw).expect("valid state json")
    }

    fn maybe_state(&self, session_id: &str) -> Option<serde_json::Value> {
        let raw = std::fs::read(self.state_dir.join(format!("{session_id}.json"))).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn write_state(&self, value: &serde_json::Value) {
        std::fs::create_dir_all(&self.state_dir).unwrap();
        let id = value["session_id"].as_str().unwrap();
        std::fs::write(self.state_dir.join(format!("{id}.json")), value.to_string()).unwrap();
    }

    /// Appends `n` user turns to the transcript and returns its new size.
    fn append_turns(&self, n: usize) -> u64 {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .unwrap();
        for i in 0..n {
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "message": {"role": "user", "content": format!("turn {i}")},
                })
            )
            .unwrap();
        }
        std::fs::metadata(&self.transcript).unwrap().len()
    }
}

/// A `SessionState` on disk, in the shape `state::load` accepts.
fn state_file(session_id: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut value = serde_json::json!({
        "schema": 1,
        "session_id": session_id,
        "bank_id": "claude-code::demo-project",
        "transcript_path": "", "cwd": "",
        "offset": 0, "chunk": 0, "turns": 0, "turns_since_retain": 0,
        "compactions": 0, "pending": null,
        "transport_failures": 0, "reject_failures": 0,
        "breaker_open_until_ms": 0, "poisoned_at": null,
    });
    for (k, v) in extra.as_object().unwrap() {
        value[k] = v.clone();
    }
    value
}

fn assert_exit_zero(out: &Output, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}: exit {:?}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `Stop` and `SessionEnd` both have `never` in the plan's stdout table.
fn assert_silent(out: &Output, what: &str) {
    assert_exit_zero(out, what);
    assert!(
        out.stdout.is_empty(),
        "{what} wrote to stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ------------------------------------------------------------------ tests

/// **The turn gate: 9 of every 10 `Stop`s open no socket at all.**
///
/// Asserted on `accepts`, not on requests: a hook that connected and then
/// decided not to ask has already paid the connect, and the 0.30 ms gated-path
/// measurement is a measurement of a hook that does not connect.
#[test]
fn the_turn_gate_retains_on_the_tenth_stop_and_connects_on_no_other() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(3);
    f.write_state(&state_file("s1", serde_json::json!({})));

    for turn in 1..=9 {
        f.stop("s1", &s.url);
        assert_eq!(s.accepts(), 0, "turn {turn} opened a socket");
        let st = f.state("s1");
        assert_eq!(st["turns"], turn);
        assert_eq!(st["turns_since_retain"], turn);
        assert_eq!(st["offset"], 0, "turn {turn} moved the cursor");
    }

    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 1);
    let st = f.state("s1");
    assert_eq!(st["turns"], 10);
    assert_eq!(st["turns_since_retain"], 0, "the cadence restarts");
    assert!(st["offset"].as_u64().unwrap() > 0);
}

/// `--force` bypasses the gate and **only** the gate.
#[test]
fn force_retains_on_the_first_stop() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"transcript_path": f.transcript.to_string_lossy()}),
    ));

    let out = f.run(
        &[
            "hook",
            "retain",
            "--force",
            "--session",
            "s1",
            "--end-reason",
            "clear",
        ],
        b"",
        &s.url,
    );
    assert_silent(&out, "forced retain");
    // The retain, then the `end_reason` update — and nothing else.
    assert_eq!(
        s.lines(),
        vec![
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
            "POST /v1/banks/claude-code%3A%3Ademo-project/sessions",
        ]
    );
    let end = s.body(1);
    assert_eq!(end["end_reason"], "clear");
    assert!(end["ended_at"].as_i64().unwrap() > 0);
    // `turns` is untouched: `SessionEnd` is not a `Stop`.
    assert_eq!(f.state("s1")["turns"], 0);
}

/// The accept table's first row: `202` advances **and** records `pending`,
/// and the body carries every field the daemon's `RetainRequest` reads.
#[test]
fn an_accepted_retain_advances_the_cursor_and_records_the_job() {
    let f = fixture("");
    let s = stub(|_, _| accepted("job-abc"));
    let size = f.append_turns(4);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "turns": 10, "compactions": 2}),
    ));

    f.stop("s1", &s.url);
    let body = s.body(0);
    assert_eq!(body["session_id"], "s1");
    assert_eq!(
        body["is_initial"], true,
        "offset 0 is a session's first retain"
    );
    assert_eq!(body["document_id"], "s1", "chunk 0 is the bare session id");
    assert_eq!(body["byte_offset"], size);
    assert_eq!(body["turn"], 11);
    assert_eq!(body["chunk"], 0);
    // Plural and cumulative — the daemon has no `compaction` field, and a key
    // it does not recognise is silently dropped.
    assert_eq!(body["compactions"], 2);
    assert_eq!(body["cwd"], "/repo/sub");
    assert_eq!(body["metadata"]["transcript_bytes"], size);
    assert_eq!(body["metadata"]["truncated"], false);
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    assert!(body["event_date"].as_i64().unwrap() > 0);

    let st = f.state("s1");
    assert_eq!(st["offset"], size);
    assert_eq!(st["chunk"], 1);
    assert_eq!(st["pending"]["job_id"], "job-abc");
    assert_eq!(st["pending"]["offset_from"], 0);
    assert_eq!(st["pending"]["offset_to"], size);
    assert_eq!(st["pending"]["chunk_before"], 0);
}

/// **`duplicate` and `skipped` both advance, and neither records a job.**
///
/// `skipped` is the one that wedged the earlier draft: `plan_ingest` returns
/// `None` for an empty role-filtered set, which is ordinary with
/// `include_tool_calls = false`. Not advancing re-sent the same delta forever
/// and poisoned the session after ten tries — losing every *subsequent* real
/// delta.
#[test]
fn duplicate_and_skipped_both_advance_without_a_pending_job() {
    for status in ["duplicate", "skipped"] {
        let f = fixture("");
        let s = stub(move |_, _| settled(status));
        let size = f.append_turns(2);
        f.write_state(&state_file(
            "s1",
            serde_json::json!({"turns_since_retain": 10, "chunk": 3}),
        ));

        f.stop("s1", &s.url);
        let st = f.state("s1");
        assert_eq!(st["offset"], size, "{status} must advance the cursor");
        assert_eq!(st["pending"], serde_json::Value::Null, "{status}");
        // The chunk moves on **every** accept: reusing a `document_id` is the
        // provenance overwrite §Binding decisions #7 exists to prevent, and
        // the daemon has already committed a row under this one.
        assert_eq!(st["chunk"], 4, "{status}");
        assert_eq!(st["turns_since_retain"], 0, "{status}");
        // The suffix, on the wire.
        assert_eq!(s.body(0)["document_id"], "s1-c3", "{status}");
    }
}

/// **The reconcile protocol, all three settlements.**
#[test]
fn a_failed_job_rolls_the_cursor_back_and_the_same_bytes_are_re_sent() {
    let f = fixture("");
    // Request 0 is the reconcile GET (failed), request 1 the re-send.
    let s = stub(|_, request| match route(request) {
        "job" => job("failed"),
        _ => accepted("j2"),
    });
    let size = f.append_turns(4);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({
            "offset": size, "chunk": 1, "turns_since_retain": 10,
            "pending": {"job_id": "j1", "offset_from": 0, "offset_to": size, "chunk_before": 0},
        }),
    ));

    f.stop("s1", &s.url);
    assert_eq!(
        s.lines(),
        vec![
            "GET /v1/retain/j1",
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
        ]
    );
    // The rollback put the cursor back to `offset_from`, so the re-send
    // carries the **same** bytes and the **same** document id.
    let resend = s.body(1);
    assert_eq!(resend["messages"].as_array().unwrap().len(), 4);
    assert_eq!(resend["document_id"], "s1", "chunk_before was 0");
    assert_eq!(resend["is_initial"], true);

    let st = f.state("s1");
    assert_eq!(st["offset"], size);
    assert_eq!(st["chunk"], 1);
    assert_eq!(st["pending"]["job_id"], "j2");
}

#[test]
fn a_done_job_clears_the_pending_record_and_the_cursor_stays_put() {
    let f = fixture("");
    let s = stub(|_, request| match route(request) {
        "job" => job("done"),
        _ => accepted("j2"),
    });
    let size = f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({
            "offset": size, "chunk": 1, "turns_since_retain": 10,
            "pending": {"job_id": "j1", "offset_from": 0, "offset_to": size, "chunk_before": 0},
        }),
    ));

    f.stop("s1", &s.url);
    // Reconciled, then nothing left to send — the cursor is already at EOF.
    assert_eq!(s.lines(), vec!["GET /v1/retain/j1"]);
    let st = f.state("s1");
    assert_eq!(st["pending"], serde_json::Value::Null);
    assert_eq!(st["offset"], size);
    assert_eq!(st["chunk"], 1);
}

/// A job that is still working **skips the turn**: stacking a second
/// unconfirmed job on an unconfirmed cursor is what makes `confirmed_offset`
/// unable to say which bytes are missing.
#[test]
fn a_running_job_skips_the_turn_without_a_second_post() {
    for status in ["pending", "running"] {
        let f = fixture("");
        let s = stub(move |_, request| match route(request) {
            "job" => job(status),
            _ => accepted("j2"),
        });
        let size = f.append_turns(2);
        f.append_turns(2);
        f.write_state(&state_file(
            "s1",
            serde_json::json!({
                "offset": size, "chunk": 1, "turns_since_retain": 10,
                "pending": {"job_id": "j1", "offset_from": 0, "offset_to": size, "chunk_before": 0},
            }),
        ));

        f.stop("s1", &s.url);
        assert_eq!(s.lines(), vec!["GET /v1/retain/j1"], "{status}");
        let st = f.state("s1");
        assert_eq!(st["pending"]["job_id"], "j1", "{status}: still in flight");
        assert_eq!(st["offset"], size, "{status}");
        // The turn was still counted — a stalled job must not freeze the gate.
        assert_eq!(st["turns"], 1, "{status}");
    }
}

/// A reconcile the daemon cannot answer leaves the record alone, skips the
/// turn, **and counts a transport failure** — without that, a down daemon
/// during reconciliation never opens the breaker and every `Stop` pays a fresh
/// connect for the rest of the session.
#[test]
fn an_unanswerable_reconcile_skips_the_turn_and_counts_a_transport_failure() {
    let f = fixture("");
    let s = stub(|_, _| error("500 Internal Server Error"));
    let size = f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({
            "offset": 0, "turns_since_retain": 10,
            "pending": {"job_id": "j1", "offset_from": 0, "offset_to": size, "chunk_before": 0},
        }),
    ));

    f.stop("s1", &s.url);
    assert_eq!(s.lines(), vec!["GET /v1/retain/j1"]);
    let st = f.state("s1");
    assert_eq!(st["pending"]["job_id"], "j1");
    assert_eq!(st["transport_failures"], 1);
}

/// **A job row that is gone is `failed`, not `unsettled`.** The plan's
/// reconcile has no arm for a 404 and would skip the turn forever, wedging the
/// session's cursor for the rest of its life over a row a database wipe took.
#[test]
fn a_missing_job_row_rolls_back_rather_than_wedging_the_session() {
    let f = fixture("");
    let s = stub(|_, request| match route(request) {
        "job" => error("404 Not Found"),
        _ => accepted("j2"),
    });
    let size = f.append_turns(3);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({
            "offset": size, "chunk": 1, "turns_since_retain": 10,
            "pending": {"job_id": "j1", "offset_from": 0, "offset_to": size, "chunk_before": 0},
        }),
    ));

    f.stop("s1", &s.url);
    assert_eq!(
        s.lines(),
        vec![
            "GET /v1/retain/j1",
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
        ]
    );
    assert_eq!(s.body(1)["messages"].as_array().unwrap().len(), 3);
}

/// A rejected POST leaves the cursor where it was, and the next success sends
/// the **union** — the delta that was refused plus everything appended since.
#[test]
fn a_rejected_post_leaves_the_cursor_and_a_later_success_sends_the_union() {
    let f = fixture("");
    let s = stub(|n, _| {
        if n == 0 {
            error("400 Bad Request")
        } else {
            accepted("j1")
        }
    });
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));

    f.stop("s1", &s.url);
    let st = f.state("s1");
    assert_eq!(st["offset"], 0, "a rejection must not advance the cursor");
    assert_eq!(st["reject_failures"], 1);
    assert_eq!(st["transport_failures"], 0, "a 4xx is not an outage");

    // Two more turns land, and the gate opens again.
    let size = f.append_turns(2);
    let mut st = f.state("s1");
    st["turns_since_retain"] = serde_json::json!(10);
    f.write_state(&st);
    f.stop("s1", &s.url);

    let union = s.body(1);
    assert_eq!(union["messages"].as_array().unwrap().len(), 4, "the union");
    assert_eq!(union["byte_offset"], size);
    assert_eq!(f.state("s1")["offset"], size);
    // A success clears both counters, which is what makes poisoning a
    // slow-retry state rather than a latch.
    assert_eq!(f.state("s1")["reject_failures"], 0);
}

/// `429` is transport-class and **never poisons**: the daemon is busy, not
/// offended, and a queue-full answer that could poison a session would turn
/// load into permanent loss.
#[test]
fn a_429_never_poisons_however_many_times_it_arrives() {
    let f = fixture("breaker_failures = 100");
    let s = stub(|_, _| error("429 Too Many Requests"));
    f.append_turns(2);

    for _ in 0..12 {
        let mut st = f
            .maybe_state("s1")
            .unwrap_or_else(|| state_file("s1", serde_json::json!({})));
        st["turns_since_retain"] = serde_json::json!(10);
        f.write_state(&st);
        f.stop("s1", &s.url);
    }
    let st = f.state("s1");
    assert_eq!(st["transport_failures"], 12);
    assert_eq!(st["reject_failures"], 0);
    assert_eq!(st["poisoned_at"], serde_json::Value::Null);
    assert_eq!(st["offset"], 0);
}

/// `503` moves **neither** counter. A 9 s model load must not blind a session
/// for a 60 s breaker cooldown, and it is not a client-side rejection either.
#[test]
fn a_503_moves_no_counter_at_all() {
    let f = fixture("");
    let s = stub(|_, _| error("503 Service Unavailable"));
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));

    f.stop("s1", &s.url);
    let st = f.state("s1");
    assert_eq!(st["transport_failures"], 0);
    assert_eq!(st["reject_failures"], 0);
    assert_eq!(st["breaker_open_until_ms"], 0);
    assert_eq!(st["offset"], 0, "the cursor still does not advance");
}

/// **Ten durable 4xx poison; the eleventh call inside the window makes no
/// request and outside it makes exactly one.**
#[test]
fn ten_rejections_poison_and_the_retry_is_hourly_rather_than_per_turn() {
    let f = fixture("");
    let s = stub(|_, _| error("422 Unprocessable Entity"));
    f.append_turns(2);

    for n in 1..=10 {
        let mut st = f
            .maybe_state("s1")
            .unwrap_or_else(|| state_file("s1", serde_json::json!({})));
        st["turns_since_retain"] = serde_json::json!(10);
        f.write_state(&st);
        f.stop("s1", &s.url);
        assert_eq!(f.state("s1")["reject_failures"], n);
    }
    assert_eq!(s.accepts(), 10);
    let poisoned_at = f.state("s1")["poisoned_at"].as_i64().expect("poisoned");

    // The eleventh, inside the window: no socket.
    let mut st = f.state("s1");
    st["turns_since_retain"] = serde_json::json!(10);
    f.write_state(&st);
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 10, "a poisoned session must not connect");

    // And outside it: exactly one. `poison_retry_secs` is 3600, so a stamp an
    // hour and a second old is out of the window.
    let mut st = f.state("s1");
    st["turns_since_retain"] = serde_json::json!(10);
    st["poisoned_at"] = serde_json::json!(poisoned_at - 3_600_001i64);
    f.write_state(&st);
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 11, "the throttle is a slow retry, not a latch");
}

/// A `poisoned_at` from the future is not a throttle. No attacker required:
/// an NTP step, a VM resume or a dual-boot RTC produces one, and the bare
/// `now < poisoned_at + window` reading disables the session **forever**.
#[test]
fn a_future_poisoned_at_does_not_disable_the_session() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    let far_future = 4_000_000_000_000i64; // year 2096, in ms
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "poisoned_at": far_future}),
    ));

    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 1, "a future stamp wedged retain off forever");
    assert_eq!(f.state("s1")["poisoned_at"], serde_json::Value::Null);
}

/// The same shape on the breaker, and the far edge of its window from both
/// sides — the mutation that drops the upper conjunct is invisible to a test
/// that only checks "open while inside".
#[test]
fn the_breaker_skips_the_socket_inside_its_window_and_not_beyond_it() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Inside a window we could have written: no socket.
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "breaker_open_until_ms": now + 30_000}),
    ));
    f.stop("s1", &s.url);
    assert_eq!(
        s.accepts(),
        0,
        "the breaker must skip the socket, not the work"
    );

    // A stamp beyond one cooldown could not have come from us: closed.
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "breaker_open_until_ms": now + 3_600_000}),
    ));
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 1);
}

/// Three transport failures open the breaker, and the fourth `Stop` opens no
/// socket at all.
#[test]
fn three_transport_failures_open_the_breaker() {
    let f = fixture("");
    // No listener: `connect()` refuses. The port is bound and dropped, so
    // nothing on this machine is holding it.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);

    for n in 1..=3 {
        let mut st = f
            .maybe_state("s1")
            .unwrap_or_else(|| state_file("s1", serde_json::json!({})));
        st["turns_since_retain"] = serde_json::json!(10);
        f.write_state(&st);
        f.stop("s1", &dead);
        assert_eq!(f.state("s1")["transport_failures"], n);
    }
    assert!(f.state("s1")["breaker_open_until_ms"].as_i64().unwrap() > 0);

    // The fourth, pointed at a *live* stub: the breaker is what stops it, not
    // the dead port.
    let mut st = f.state("s1");
    st["turns_since_retain"] = serde_json::json!(10);
    f.write_state(&st);
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 0);
}

/// A `404` creates the bank and retries **exactly once**. A second 404 after
/// the create is durable, and retrying it every turn is how a self-heal
/// becomes a hot loop.
#[test]
fn a_missing_bank_is_created_once_and_the_retain_retried_once() {
    let f = fixture("");
    let s = stub(|n, request| match (route(request), n) {
        ("retain", 0) => error("404 Not Found"),
        ("retain", _) => accepted("j1"),
        _ => json_reply("200 OK", "{}"),
    });
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));

    f.stop("s1", &s.url);
    assert_eq!(
        s.lines(),
        vec![
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
            "POST /v1/banks",
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
        ]
    );
    assert_eq!(s.body(1)["bank_id"], "claude-code::demo-project");
    // No `mission`: the daemon owns mission precedence (C2b).
    assert!(s.body(1).get("mission").is_none());
    assert!(f.state("s1")["offset"].as_u64().unwrap() > 0);

    // A bank that stays missing is a durable rejection, counted once.
    let f2 = fixture("");
    let s2 = stub(|_, request| match route(request) {
        "retain" => error("404 Not Found"),
        _ => json_reply("200 OK", "{}"),
    });
    f2.append_turns(2);
    f2.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));
    f2.stop("s1", &s2.url);
    assert_eq!(s2.retains().len(), 2, "exactly one retry, never a loop");
    assert_eq!(f2.state("s1")["reject_failures"], 1);
}

/// **`compaction` is a counter and nothing else.** It does not reset the
/// offset, it does not drive `chunk`, and the messages on both sides of a
/// boundary survive in order.
#[test]
fn a_compaction_is_counted_and_never_drives_the_chunk() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(1);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&f.transcript)
        .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type": "system", "subtype": "compact_boundary"})
    )
    .unwrap();
    drop(file);
    let size = f.append_turns(1);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "chunk": 2, "compactions": 1}),
    ));

    f.stop("s1", &s.url);
    let body = s.body(0);
    // Cumulative: the state file's 1 plus this delta's 1.
    assert_eq!(body["compactions"], 2);
    assert_eq!(body["chunk"], 2, "a compaction does not move the chunk");
    assert_eq!(body["document_id"], "s1-c2");
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    let st = f.state("s1");
    assert_eq!(st["compactions"], 2);
    // One accepted delta, one chunk — not two.
    assert_eq!(st["chunk"], 3);
    assert_eq!(st["offset"], size, "the boundary did not reset the offset");
}

/// A delta that consumed only lines the reader skips **still advances**. Not
/// advancing rescans them on every retain turn for the rest of the session.
#[test]
fn a_delta_with_nothing_to_send_advances_the_cursor_without_a_post() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    let mut file = std::fs::File::create(&f.transcript).unwrap();
    for _ in 0..3 {
        writeln!(
            file,
            "{}",
            serde_json::json!({"type": "file-history-snapshot", "payload": "skipped"})
        )
        .unwrap();
    }
    drop(file);
    let size = std::fs::metadata(&f.transcript).unwrap().len();
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));

    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 0, "there was nothing to post");
    let st = f.state("s1");
    assert_eq!(st["offset"], size);
    assert_eq!(
        st["chunk"], 0,
        "nothing was accepted, so nothing is a chunk"
    );
    assert_eq!(st["turns_since_retain"], 0, "the cadence restarts");
}

/// A transcript that was rewritten shorter resets the cursor; one that has not
/// grown is not read at all.
#[test]
fn a_cursor_past_the_end_resets_and_a_caught_up_one_makes_no_request() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    let size = f.append_turns(2);

    // Caught up: no request, no cursor movement.
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "offset": size}),
    ));
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 0);
    assert_eq!(f.state("s1")["offset"], size);

    // Past the end: reset to 0 and re-send the whole file as an initial retain.
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10, "offset": size + 9_000}),
    ));
    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 1);
    assert_eq!(s.body(0)["is_initial"], true);
    assert_eq!(s.body(0)["messages"].as_array().unwrap().len(), 2);
}

/// **The transcript is validated by property at the read.** A path that is not
/// a regular file is refused wherever it came from, and nothing is posted.
#[test]
fn a_transcript_path_that_is_not_a_regular_file_is_refused() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);

    // A directory, named exactly like a transcript.
    let dir_path = f._tmp.path().join("masquerade.jsonl");
    std::fs::create_dir(&dir_path).unwrap();
    let payload = serde_json::json!({
        "session_id": "s1",
        "transcript_path": dir_path.to_string_lossy(),
        "cwd": "/repo",
        "hook_event_name": "Stop",
    })
    .to_string();
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"turns_since_retain": 10}),
    ));
    let out = f.run(&["hook", "retain"], payload.as_bytes(), &s.url);
    assert_silent(&out, "directory transcript");
    assert_eq!(s.accepts(), 0);
    assert_eq!(f.state("s1")["offset"], 0);

    // And a path that does not exist at all.
    let payload = serde_json::json!({
        "session_id": "s1",
        "transcript_path": "/nonexistent/transcript.jsonl",
        "hook_event_name": "Stop",
    })
    .to_string();
    let out = f.run(&["hook", "retain"], payload.as_bytes(), &s.url);
    assert_silent(&out, "absent transcript");
    assert_eq!(s.accepts(), 0);
}

/// The payload's path wins over the stored one on every `Stop`, which is why
/// the guard is at the read: a store-time check would cover the once-per-
/// session path and leave this one open.
#[test]
fn the_stop_path_reads_the_payloads_transcript_not_the_stored_one() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    // A stale stored path that no longer exists.
    f.write_state(&state_file(
        "s1",
        serde_json::json!({
            "turns_since_retain": 10,
            "transcript_path": "/gone/old.jsonl",
        }),
    ));

    f.stop("s1", &s.url);
    assert_eq!(s.accepts(), 1, "the payload's path is the one that is read");
    // …and it is written back, so the detached children inherit it.
    assert_eq!(
        f.state("s1")["transcript_path"],
        f.transcript.to_string_lossy().to_string()
    );
    assert_eq!(f.state("s1")["cwd"], "/repo/sub");
}

/// `hook session-end` spawns the detached child and **posts nothing itself**.
#[test]
fn session_end_spawns_a_detached_child_and_makes_no_request_of_its_own() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"transcript_path": f.transcript.to_string_lossy()}),
    ));

    let payload = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "SessionEnd",
        "reason": "prompt_input_exit",
    })
    .to_string();
    let out = f.run(&["hook", "session-end"], payload.as_bytes(), &s.url);
    assert_silent(&out, "session-end");

    // The child is detached, so the parent's exit says nothing about it. Wait
    // for the work to land rather than sleeping a fixed amount.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while s.requests().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        s.lines(),
        vec![
            "POST /v1/banks/claude-code%3A%3Ademo-project/retain",
            "POST /v1/banks/claude-code%3A%3Ademo-project/sessions",
        ],
        "the child retains and reports the end reason; the parent does neither"
    );
    assert_eq!(s.body(1)["end_reason"], "prompt_input_exit");
    // The retain was forced: the state file's `turns_since_retain` was 0.
    assert_eq!(s.body(0)["is_initial"], true);
}

/// An 8 MB `reason` is bounded before it becomes an argv element — an `E2BIG`
/// on the `execve` is a lost final retain, one layer further out than the
/// daemon's own 64-byte check can reach.
#[test]
fn an_absurd_end_reason_does_not_cost_the_child_its_execve() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    f.write_state(&state_file(
        "s1",
        serde_json::json!({"transcript_path": f.transcript.to_string_lossy()}),
    ));

    let payload = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "SessionEnd",
        "reason": "x".repeat(4 * 1024 * 1024),
    })
    .to_string();
    let out = f.run(&["hook", "session-end"], payload.as_bytes(), &s.url);
    assert_silent(&out, "session-end with an absurd reason");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while s.requests().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(s.requests().len(), 2, "the child still ran");
    let end = s.body(1);
    let reason = end["end_reason"].as_str().expect("a reason");
    assert_eq!(reason.len(), 64, "bounded to the daemon's own limit");
}

/// The forced child with no state file has no bank to post to and nothing to
/// do. It must exit 0 and touch nothing rather than invent a bank.
#[test]
fn the_forced_child_without_a_state_file_does_nothing() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);

    let out = f.run(
        &[
            "hook",
            "retain",
            "--force",
            "--session",
            "ghost",
            "--end-reason",
            "clear",
        ],
        b"",
        &s.url,
    );
    assert_silent(&out, "forced child with no state");
    assert_eq!(s.accepts(), 0);
    assert!(f.maybe_state("ghost").is_none());
}

/// **Catch-up posts, and it re-checks under the lock.** A session whose cursor
/// was already moved past the file by the time the child got to it is skipped
/// rather than re-sent from a stale snapshot.
#[test]
fn catchup_posts_each_selected_session_and_re_checks_under_the_lock() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    let size = f.append_turns(3);
    // `behind` has bytes to send; `caught-up`'s cursor is already at EOF.
    for (id, offset) in [("behind", 0u64), ("caught-up", size)] {
        f.write_state(&state_file(
            id,
            serde_json::json!({
                "transcript_path": f.transcript.to_string_lossy(),
                "cwd": "/repo/from-state",
                "offset": offset,
            }),
        ));
    }

    let out = f.run(&["hook", "catchup", "current"], b"", &s.url);
    assert_exit_zero(&out, "catchup");
    assert_eq!(s.retains().len(), 1, "only the session that is behind");
    let body = &s.retains()[0];
    assert_eq!(body["session_id"], "behind");
    // The rule the plan's C2b line gets wrong: catch-up at offset 0 **is** a
    // session's first retain, and hardcoding `false` takes the daemon's
    // uncapped branch on the largest payload in the system.
    assert_eq!(body["is_initial"], true);
    // The stored `cwd`, which is the whole reason the field exists: a child
    // with no payload would otherwise post `null` and get absolute `file:`
    // tags for files the live hook tagged relatively.
    assert_eq!(body["cwd"], "/repo/from-state");
    assert_eq!(f.state("behind")["offset"], size);
    assert_eq!(f.state("caught-up")["offset"], size);
}

/// `--dry-run` stays observation-only now that the child posts.
#[test]
fn a_dry_run_catchup_still_posts_nothing() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(3);
    f.write_state(&state_file(
        "behind",
        serde_json::json!({"transcript_path": f.transcript.to_string_lossy()}),
    ));

    let out = f.run(&["hook", "catchup", "current", "--dry-run"], b"", &s.url);
    assert_exit_zero(&out, "catchup --dry-run");
    assert!(String::from_utf8_lossy(&out.stdout).contains("selected 1"));
    assert_eq!(s.accepts(), 0);
    assert_eq!(f.state("behind")["offset"], 0);
}

/// The config switch, on this PR's two subcommands: no request, no state.
#[test]
fn the_config_switch_makes_no_request_and_writes_no_state() {
    let f = fixture("enabled = false");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);

    let out = f.run(&["hook", "retain"], &f.payload("s1"), &s.url);
    assert_silent(&out, "disabled retain");
    let out = f.run(
        &["hook", "session-end"],
        br#"{"session_id":"s1","reason":"clear"}"#,
        &s.url,
    );
    assert_silent(&out, "disabled session-end");

    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(s.accepts(), 0);
    assert!(f.maybe_state("s1").is_none());
}

/// Two of our own processes on one cursor — the only race the advisory lock
/// can arbitrate, and the one `async: true` on the `Stop` entry creates.
#[test]
fn concurrent_stops_serialize_and_neither_turn_is_lost() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    f.append_turns(2);
    f.write_state(&state_file("s1", serde_json::json!({})));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let config = f.config.clone();
            let project = f.project.clone();
            let home = f.home.clone();
            let payload = f.payload("s1");
            let url = s.url.clone();
            std::thread::spawn(move || {
                let mut child = Command::new(env!("CARGO_BIN_EXE_memgarden"))
                    .args(["hook", "retain"])
                    .env("MEMGARDEN_CONFIG", &config)
                    .env("MEMGARDEN_DAEMON_URL", &url)
                    .env("CLAUDE_PROJECT_DIR", &project)
                    .env("HOME", &home)
                    .env("XDG_DATA_HOME", home.join("data"))
                    .env_remove("MEMGARDEN_HOOKS_DISABLE")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn");
                let _ = child.stdin.take().unwrap().write_all(&payload);
                child.wait_with_output().unwrap()
            })
        })
        .collect();
    for h in handles {
        assert_exit_zero(&h.join().unwrap(), "concurrent stop");
    }
    // Four increments, none lost to an unserialized read-modify-write.
    assert_eq!(f.state("s1")["turns"], 4);
    assert_eq!(s.accepts(), 0, "none of the four reached the gate");
}

/// The guarantee, on this PR's argv shapes.
#[test]
fn every_retain_argv_shape_exits_zero() {
    let f = fixture("");
    let s = stub(|_, _| accepted("j1"));
    for args in [
        vec!["hook", "retain"],
        vec!["hook", "retain", "--force"],
        vec!["hook", "retain", "--force", "--session"],
        vec!["hook", "retain", "--session", "s1"],
        vec!["hook", "retain", "--end-reason", "clear"],
        vec!["hook", "session-end"],
        vec!["hook", "session-end", "--nonsense"],
    ] {
        let out = f.run(&args, b"", &s.url);
        assert_silent(&out, &format!("{args:?}"));
        assert_ne!(out.status.code(), Some(2), "{args:?}");
    }
    // …and with a payload the parser cannot use.
    for stdin in [&b"not json"[..], b"[1,2,3]", b"{\"session_id\": 42}"] {
        let out = f.run(&["hook", "retain"], stdin, &s.url);
        assert_silent(&out, "malformed stdin");
    }
}
