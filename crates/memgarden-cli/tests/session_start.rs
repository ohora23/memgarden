//! `hook session-start` against a real socket and the real binary.
//!
//! The first subcommand that reads stdin, loads config and makes a request, so
//! this is the first file that can assert any of those end to end. Everything
//! here drives `CARGO_BIN_EXE_memgarden` as a child process: the guarantee
//! under test is about a *process*, and a `dispatch()` call cannot observe an
//! exit code, a detached grandchild, or an empty stdout.
//!
//! Every listener binds port **0**. 9077 (legacy hindsight) and 9090 (memdash)
//! are live on this machine and are never touched (plan §Cross-PR rules 1).
//! Nothing here reads the user's real config, data dir or `settings.json`:
//! `MEMGARDEN_CONFIG`, `HOME` and `XDG_DATA_HOME` are all redirected into a
//! temp dir, and `state_dir` is pinned explicitly on top of that.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- the stub

struct Stub {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// A sequential stub daemon. `reply(n, request)` produces the raw response for
/// the `n`-th request, so a test can make the bank POST answer 201 once and
/// 409 forever after — which is exactly what the real daemon does.
fn stub(reply: impl Fn(usize, &str) -> String + Send + 'static) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorder = requests.clone();
    std::thread::spawn(move || {
        for (n, sock) in listener.incoming().enumerate() {
            let Ok(mut sock) = sock else { continue };
            let request = read_request(&mut sock);
            let response = reply(n, &request);
            recorder.lock().unwrap().push(request);
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    });
    Stub { url, requests }
}

/// Reads one whole request: head, then exactly `Content-Length` body bytes.
/// A single `read` would usually get both — the client writes one buffer — but
/// "usually" is how a test becomes flaky on a loaded machine.
fn read_request(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
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

fn json_reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The `sessions` upsert's answer: a `SessionResponse` (C1). It carries
/// **both** cursors, which is the whole reason the hook's own struct omits one.
fn mirror_reply(byte_offset: i64, confirmed_offset: i64, chunk_index: i64) -> String {
    json_reply(
        "200 OK",
        &serde_json::json!({
            "bank_id": "claude-code::demo-project",
            "session_id": "s1",
            "cwd": null,
            "transcript_path": null,
            "source": "startup",
            "end_reason": null,
            "turns": 20,
            "retains": 3,
            "chunk_index": chunk_index,
            "byte_offset": byte_offset,
            "confirmed_offset": confirmed_offset,
            "inflight_bytes": byte_offset - confirmed_offset,
            "messages_sent": 120,
            "compactions": 1,
            "started_at": 1,
            "last_seen_at": 2,
            "ended_at": null,
        })
        .to_string(),
    )
}

/// Answers every request the way a healthy daemon would.
fn healthy_stub() -> Stub {
    stub(|n, _| {
        if n == 0 {
            json_reply("201 Created", r#"{"bank_id":"claude-code::demo-project"}"#)
        } else {
            mirror_reply(0, 0, 0)
        }
    })
}

// ------------------------------------------------------------- the fixture

struct Fixture {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    state_dir: PathBuf,
    project: PathBuf,
    home: PathBuf,
}

/// A hermetic environment: its own config file, state dir, `HOME`, data dir and
/// git repo. `extra` is appended to the `[hooks]` table.
fn fixture(extra: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let project = tmp.path().join("demo-project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let config = tmp.path().join("memgarden.toml");
    std::fs::write(
        &config,
        format!(
            "[hooks]\nstate_dir = {:?}\n{extra}\n",
            state_dir.to_string_lossy()
        ),
    )
    .unwrap();
    Fixture {
        _tmp: tmp,
        config,
        state_dir,
        project,
        home,
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
            // Inherited from whoever runs `cargo test`; it would short-circuit
            // every one of these before the subcommand ran.
            .env_remove("MEMGARDEN_HOOKS_DISABLE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn memgarden");
        let _ = child.stdin.take().expect("stdin").write_all(stdin);
        child.wait_with_output().expect("wait")
    }

    fn state(&self, session_id: &str) -> Option<serde_json::Value> {
        let raw = std::fs::read(self.state_dir.join(format!("{session_id}.json"))).ok()?;
        serde_json::from_slice(&raw).ok()
    }
}

/// A minimal but real `SessionStart` payload.
fn payload(session_id: &str, transcript_path: &str) -> Vec<u8> {
    serde_json::json!({
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": "/repo/sub",
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string()
    .into_bytes()
}

fn assert_silent_success(out: &Output, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}: exit {:?}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // `SessionStart` stdout is model context (plan §Binding decisions #3).
    assert!(
        out.stdout.is_empty(),
        "{what} wrote to stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

// ------------------------------------------------------------------ tests

/// The two POSTs, in order, with the bank id percent-encoded into the path —
/// and a 409 on the second run, which is the answer every session after the
/// first gets and must not be treated as a failure.
#[test]
fn the_bank_is_created_on_the_first_run_and_a_409_is_not_a_failure() {
    let f = fixture("");
    let s = stub(|n, _| match n {
        0 => json_reply("201 Created", r#"{"bank_id":"claude-code::demo-project"}"#),
        2 => json_reply("409 Conflict", r#"{"error":{"code":"conflict"}}"#),
        _ => mirror_reply(0, 0, 0),
    });

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "first",
    );
    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s2", ""), &s.url),
        "second",
    );

    let requests = s.requests();
    assert_eq!(requests.len(), 4, "{requests:#?}");
    assert!(
        requests[0].starts_with("POST /v1/banks HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].ends_with(r#"{"bank_id":"claude-code::demo-project"}"#),
        "{}",
        requests[0]
    );
    // `::` must be escaped or the daemon 400s; a raw space in a bank id would
    // additionally split the request line into four tokens.
    assert!(
        requests[1]
            .starts_with("POST /v1/banks/claude-code%3A%3Ademo-project/sessions HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
    assert_eq!(
        requests[1].split("\r\n").next().unwrap().split(' ').count(),
        3,
        "the request line must be exactly three tokens"
    );
    // The 409 did not stop the second session from being mirrored.
    assert!(
        requests[3].contains(r#""session_id":"s2""#),
        "{}",
        requests[3]
    );
    assert!(
        f.state("s2").is_some(),
        "a 409 on the bank must still leave state"
    );
}

/// The wiped-state-dir recovery, and the reason C1 and C2a both wrote it down:
/// the mirror carries **both** cursors and only one of them is safe to resume
/// from. `byte_offset` here is 99999; taking it would skip 34463 bytes that
/// nothing ingested.
#[test]
fn recovery_seeds_the_offset_from_confirmed_offset_and_never_from_byte_offset() {
    let f = fixture("");
    let s = stub(|n, _| {
        if n == 0 {
            json_reply("409 Conflict", "{}")
        } else {
            mirror_reply(99999, 65536, 2)
        }
    });

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "recover",
    );

    let state = f.state("s1").expect("state file");
    assert_eq!(state["offset"], 65536);
    assert_eq!(state["chunk"], 2);
    assert_ne!(state["offset"], 99999, "seeded from the optimistic cursor");
    // A recovered session has no in-flight job to reconcile: the mirror
    // carries no job id, and `confirmed_offset` is behind anything unresolved.
    assert_eq!(state["pending"], serde_json::Value::Null);
}

/// The mirror is consulted **only** when the local file is absent. It is the
/// authoritative copy, but it is also behind by design, and a `resume` that
/// rewound a live cursor to the last settled offset would re-send every delta
/// since.
#[test]
fn an_existing_state_file_is_not_rewound_by_the_mirror() {
    let f = fixture("");
    let s = stub(|n, _| {
        if n == 0 {
            json_reply("409 Conflict", "{}")
        } else {
            mirror_reply(4096, 4096, 1)
        }
    });
    let transcript = f.state_dir.join("t.jsonl");

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "first",
    );
    // Advance the cursor the way a retain would.
    let mut state = f.state("s1").unwrap();
    state["offset"] = serde_json::json!(900_000);
    std::fs::write(
        f.state_dir.join("s1.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();

    let resumed = payload("s1", &transcript.to_string_lossy());
    assert_silent_success(
        &f.run(&["hook", "session-start"], &resumed, &s.url),
        "resume",
    );

    let state = f.state("s1").unwrap();
    assert_eq!(
        state["offset"], 900_000,
        "a resume must not rewind the cursor"
    );
    // …but the transcript path is refreshed, because catch-up has no payload.
    assert_eq!(
        state["transcript_path"],
        transcript.to_string_lossy().as_ref()
    );
}

/// C2a shipped `[hooks] enabled` inert: nothing loaded config, so the only
/// switch that worked was the env var. This is the test that makes the
/// documented knob real.
#[test]
fn the_config_switch_makes_no_request_and_writes_no_state() {
    let f = fixture("enabled = false");
    let s = healthy_stub();

    let out = f.run(&["hook", "session-start"], &payload("s1", ""), &s.url);
    assert_silent_success(&out, "disabled");
    assert!(
        out.stderr.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(s.requests().is_empty(), "a disabled hook made a request");
    assert!(f.state("s1").is_none(), "a disabled hook wrote state");

    // The child would not have anything to do either.
    let out = f.run(&["hook", "catchup", "s1", "--dry-run"], b"", &s.url);
    assert_silent_success(&out, "disabled catchup");
}

/// §Failure posture, `session-start` row: exit 0, no bank created,
/// `transport_failures += 1`. The state file still lands — losing it would
/// cost the *next* hook its bank id for no reason.
#[test]
fn a_daemon_that_is_down_exits_zero_and_counts_exactly_one_transport_failure() {
    let f = fixture("");
    // Bound and dropped: nothing is listening, so this is ECONNREFUSED rather
    // than a timeout.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &url),
        "down",
    );
    let state = f.state("s1").expect("state file");
    assert_eq!(state["transport_failures"], 1);
    assert_eq!(state["offset"], 0);
    assert_eq!(state["bank_id"], "claude-code::demo-project");

    // A second start increments again — three of these open the breaker.
    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &url),
        "down 2",
    );
    assert_eq!(f.state("s1").unwrap()["transport_failures"], 2);

    // …and any success clears it, along with the breaker window.
    let s = healthy_stub();
    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "up",
    );
    let state = f.state("s1").unwrap();
    assert_eq!(state["transport_failures"], 0);
    assert_eq!(state["breaker_open_until_ms"], 0);
}

/// A 4xx moves **no** counter here. `reject_failures` exists to poison a
/// cursor, and `session-start` does not advance one — counting rejections
/// would let a daemon-side validation bug disable a session's memory.
#[test]
fn a_rejected_mirror_moves_no_counter() {
    let f = fixture("");
    let s = stub(|_, _| json_reply("400 Bad Request", r#"{"error":{"code":"invalid"}}"#));

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "400",
    );
    let state = f.state("s1").expect("state file");
    assert_eq!(state["transport_failures"], 0);
    assert_eq!(state["reject_failures"], 0);
    assert_eq!(state["offset"], 0, "a rejection must not recover an offset");
}

/// A `daemon_url` typo is a config fault, not an outage. Counting it would
/// open the circuit breaker over a misspelling and make `hooks status` report
/// a healthy daemon as down.
#[test]
fn a_non_loopback_daemon_url_is_not_counted_as_a_transport_failure() {
    let f = fixture("");
    assert_silent_success(
        &f.run(
            &["hook", "session-start"],
            &payload("s1", ""),
            "http://example.com:9100",
        ),
        "bad url",
    );
    assert_eq!(f.state("s1").expect("state file")["transport_failures"], 0);
}

/// An id that cannot round-trip is refused before anything is written: it
/// would become an 8 MB state file, a body the daemon 400s, and an argv
/// element over `ARG_MAX`.
#[test]
fn an_unusable_session_id_writes_nothing_and_makes_no_request() {
    let f = fixture("");
    let s = healthy_stub();
    for id in ["", &"x".repeat(201)] {
        assert_silent_success(
            &f.run(&["hook", "session-start"], &payload(id, ""), &s.url),
            id,
        );
    }
    assert!(s.requests().is_empty(), "{:#?}", s.requests());
    assert_eq!(
        std::fs::read_dir(&f.state_dir)
            .map(Iterator::count)
            .unwrap_or(0),
        0
    );
}

/// The detached child, asserted the only way that means anything: a fake that
/// writes to all three streams, and then reports where those streams actually
/// went.
///
/// Both properties matter for a different reason. An inherited **stdout** on
/// `SessionStart` puts the child's bytes into the model's context *and* keeps
/// the pipe open after the parent exits, which is how a "detached" child hangs
/// its supervisor — it would have made C2a's measured 0.243 ms a fiction. A
/// shared **process group** means the terminal going away takes the child with
/// it, mid-retain.
#[test]
fn a_detached_child_gets_dev_null_on_all_three_streams_and_its_own_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let evidence = tmp.path().join("evidence.txt");
    let script = format!(
        // The three writes are the point: they must go nowhere. `cat` proves
        // stdin is not an open pipe — on an inherited one it would block here
        // forever, which is the "detached child hangs its supervisor" failure.
        //
        // The reporting has to happen in a **subshell** whose own stdout is
        // the evidence file, reading `/proc/<outer shell>/fd/*`. Measured on
        // dash: redirecting a simple command (`readlink … > file`) or a brace
        // group also moves fd 1 of the shell being inspected, so both of those
        // spellings reported `evidence.txt` for fd 1 against a child that was
        // correctly wired to `/dev/null` — a false failure that reads exactly
        // like a real leak. A subshell is a separate pid, so `$p` still names
        // the process the parent actually configured.
        "echo leaked-stdout; echo leaked-stderr >&2; cat >/dev/null; p=$$; \
         ( readlink /proc/$p/fd/0; readlink /proc/$p/fd/1; readlink /proc/$p/fd/2; \
           ps -o pgid= -p $p ) > {evidence:?}",
        evidence = evidence
    );
    memgarden_cli::cmd::spawn_detached(Path::new("/bin/sh"), &["-c", &script]);

    wait_until("the fake child to report", || evidence.exists());
    // Written in one shot by the block redirect, but the file can be observed
    // between `creat` and the write.
    let mut lines = Vec::new();
    wait_until("the fake child's four lines", || {
        lines = std::fs::read_to_string(&evidence)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .map(str::to_string)
            .collect();
        lines.len() == 4
    });

    assert_eq!(
        &lines[..3],
        ["/dev/null", "/dev/null", "/dev/null"],
        "{lines:?}"
    );

    let ours = Command::new("ps")
        .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    let ours = String::from_utf8_lossy(&ours.stdout).trim().to_string();
    assert_ne!(lines[3], ours, "the child stayed in our process group");
}

/// The child is really spawned by `session-start`, and it really runs the
/// catch-up code — observed through the one side effect it has in C2b:
/// collecting the state directory. C2a shipped `state::gc` with no caller at
/// all, so this is also the test that keeps it wired.
#[test]
fn session_start_spawns_the_child_and_the_child_collects_the_state_dir() {
    let f = fixture("");
    let s = healthy_stub();
    std::fs::create_dir_all(&f.state_dir).unwrap();
    // Older than any plausible `session_retention_days`.
    let ancient = f.state_dir.join("ancient.json");
    std::fs::write(&ancient, b"{}").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&ancient)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();

    assert_silent_success(
        &f.run(&["hook", "session-start"], &payload("s1", ""), &s.url),
        "start",
    );
    wait_until("the detached child to collect the state dir", || {
        !ancient.exists()
    });
    // The live session's own file survives it.
    assert!(f.state("s1").is_some());
}

/// The child's selection, end to end through the real binary's argv — the one
/// path `--dry-run` exists to make observable, because the production child's
/// three streams are `/dev/null`.
#[test]
fn the_child_selects_stale_sessions_and_excludes_the_one_it_was_given() {
    let f = fixture("");
    let s = healthy_stub();
    std::fs::create_dir_all(&f.state_dir).unwrap();

    // Two sessions whose transcripts have grown past their cursors, plus one
    // that is caught up.
    for id in ["stale-a", "stale-b", "done"] {
        let transcript = f.state_dir.join(format!("{id}.jsonl"));
        std::fs::write(&transcript, "x".repeat(4096)).unwrap();
        let state = serde_json::json!({
            "schema": 1,
            "session_id": id,
            "bank_id": "claude-code::demo-project",
            "transcript_path": transcript.to_string_lossy(),
            "offset": if id == "done" { 4096 } else { 0 },
            "chunk": 0, "turns": 0, "turns_since_retain": 0, "compactions": 0,
            "pending": null, "transport_failures": 0, "reject_failures": 0,
            "breaker_open_until_ms": 0, "poisoned_at": null,
        });
        std::fs::write(f.state_dir.join(format!("{id}.json")), state.to_string()).unwrap();
    }

    let out = f.run(&["hook", "catchup", "stale-a", "--dry-run"], b"", &s.url);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\nselected 1\n"), "{stdout}");
    assert!(
        stdout.contains("  stale-b bank=claude-code::demo-project offset=0 size=4096 behind=4096"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("  stale-a bank="),
        "the current session was selected: {stdout}"
    );
    assert!(
        !stdout.contains("  done bank="),
        "a caught-up session was selected: {stdout}"
    );
    // The child never talks to the daemon in C2b — it selects, and C4b posts.
    assert!(s.requests().is_empty(), "{:#?}", s.requests());
}

/// A session id is untrusted stdin and it lands in an argv slot. Review
/// demonstrated against the real binary that a `--`-prefixed id was thrown
/// away — `excluded` came back **empty**, so the live session became a
/// catch-up candidate and the exclusion filter was switched off by its own
/// subject. Both slots are fixed positions now, so an id that looks like a
/// flag is still just the id.
#[test]
fn a_session_id_that_looks_like_a_flag_is_still_the_session_id() {
    let f = fixture("");
    let s = healthy_stub();
    std::fs::create_dir_all(&f.state_dir).unwrap();

    for id in ["--dry-run", "stale-b"] {
        let transcript = f.state_dir.join(format!("{}.jsonl", id.replace('-', "_")));
        std::fs::write(&transcript, "x".repeat(4096)).unwrap();
        let state = serde_json::json!({
            "schema": 1,
            "session_id": id,
            "bank_id": "claude-code::demo-project",
            "transcript_path": transcript.to_string_lossy(),
            "offset": 0,
            "chunk": 0, "turns": 0, "turns_since_retain": 0, "compactions": 0,
            "pending": null, "transport_failures": 0, "reject_failures": 0,
            "breaker_open_until_ms": 0, "poisoned_at": null,
        });
        std::fs::write(f.state_dir.join(format!("{id}.json")), state.to_string()).unwrap();
    }

    // argv[2] is the session id `--dry-run`; argv[3] is the flag.
    let out = f.run(&["hook", "catchup", "--dry-run", "--dry-run"], b"", &s.url);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The assertion that fails on the old parse: it reported `excluded ` with
    // nothing after it, and selected 2.
    assert!(stdout.contains("\nexcluded --dry-run\n"), "{stdout}");
    assert!(stdout.contains("\nselected 1\n"), "{stdout}");
    assert!(
        !stdout.contains("  --dry-run bank="),
        "the current session was selected because its id looked like a flag: {stdout}"
    );
    assert!(stdout.contains("  stale-b bank="), "{stdout}");

    // The state file for that id is literally `--dry-run.json`. Recorded in
    // §Known limits rather than sanitized: a leading `-` is a hazard for a
    // future `find … | xargs` over the state dir, and `gc` uses full paths.
    assert!(f.state_dir.join("--dry-run.json").exists());
}
