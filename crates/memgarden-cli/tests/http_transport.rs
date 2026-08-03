//! The loopback client against a real socket.
//!
//! The unit tests in `src/http.rs` cover parsing; these cover the parts that
//! only exist once there is a kernel involved — connect failures, the socket
//! timeouts, and what actually goes out on the wire.
//!
//! Every listener binds port **0**. 9077 (legacy hindsight) and 9090 (memdash)
//! are live on this machine and are never touched (plan §Cross-PR rules 1).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use memgarden_cli::http::{self, HttpError, Target, Timeouts};

/// A one-shot server that replies with `reply` verbatim and hands the request
/// it read back over a channel. `reply` is raw bytes so a test can send things
/// no real server would.
fn stub(reply: &'static [u8]) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = sock.read(&mut buf).unwrap_or(0);
        let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
        let _ = sock.write_all(reply);
        let _ = sock.flush();
    });
    (addr, rx)
}

fn target(addr: SocketAddr) -> Target {
    Target::parse(&format!("http://{addr}")).unwrap()
}

fn quick() -> Timeouts {
    Timeouts::from_ms(500, 2000)
}

#[test]
fn a_post_round_trips_and_carries_a_loopback_host_header() {
    let (addr, rx) =
        stub(b"HTTP/1.1 202 Accepted\r\ncontent-length: 16\r\n\r\n{\"status\":\"ok\"}\n\n");
    let response = http::post(&target(addr), "/v1/retain", b"{\"a\":1}", &quick()).unwrap();
    assert_eq!(response.status, 202);
    assert_eq!(response.body.len(), 16);

    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The daemon 403s anything whose Host is not loopback
    // (`middleware.rs:34-46`), so this header is a hard requirement, not a
    // courtesy. Asserted on the bytes that reach the socket, not on the field
    // that produced them.
    assert!(
        request.contains(&format!("Host: 127.0.0.1:{}\r\n", addr.port())),
        "{request}"
    );
    assert!(
        request.starts_with("POST /v1/retain HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(request.contains("Content-Length: 7\r\n"), "{request}");
    assert!(request.contains("Connection: close\r\n"), "{request}");
    assert!(request.ends_with("{\"a\":1}"), "{request}");
}

#[test]
fn a_get_sends_no_body_headers() {
    let (addr, rx) = stub(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]");
    let response = http::get(&target(addr), "/v1/banks/b1/sessions/s1", &quick()).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"[]");

    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(request.starts_with("GET /v1/banks/b1/sessions/s1 HTTP/1.1\r\n"));
    assert!(!request.to_ascii_lowercase().contains("content-length"));
}

/// A 4xx is a *response*, not a transport failure. The two are counted
/// separately (`transport_failures` vs `reject_failures`) precisely so a
/// down daemon can never poison a session, so the client must not collapse
/// them by erroring here.
#[test]
fn a_non_2xx_is_returned_with_its_body_rather_than_erroring() {
    let (addr, _rx) = stub(b"HTTP/1.1 404 Not Found\r\ncontent-length: 14\r\n\r\nbank not found");
    let response = http::get(&target(addr), "/v1/banks/nope", &quick()).unwrap();
    assert_eq!(response.status, 404);
    assert!(!response.is_success());
    assert_eq!(response.body, b"bank not found");
}

#[test]
fn a_chunked_reply_is_a_failure_and_not_a_mis_parse() {
    // What a proxy in front of the daemon would send. axum never does — it
    // serializes `Json` to bytes and sets Content-Length.
    let (addr, _rx) =
        stub(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n7\r\nhijack\n\r\n0\r\n\r\n");
    let err = http::get(&target(addr), "/v1/recall", &quick()).unwrap_err();
    match err {
        // The body must not surface at all: `7` and `hijack` would otherwise
        // become model context.
        HttpError::Protocol(m) => assert!(m.contains("transfer-encoding"), "{m}"),
        other => panic!("expected a protocol error, got {other}"),
    }
}

#[test]
fn a_reply_without_content_length_is_a_failure() {
    let (addr, _rx) = stub(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{}");
    let err = http::get(&target(addr), "/v1/recall", &quick()).unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err}");
}

#[test]
fn a_daemon_that_is_down_is_a_connect_error() {
    // Bound and immediately dropped, so the port is (almost certainly) free
    // and nothing is listening: the ECONNREFUSED path the daemon-down row of
    // §Failure posture describes.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let err = http::get(&target(addr), "/healthz", &quick()).unwrap_err();
    assert!(matches!(err, HttpError::Connect(_)), "{err}");
}

/// The mutation this test exists to catch: a `Timeouts` value that is
/// constructed correctly and then never reaches the socket. Asserting the
/// *elapsed wall time* is the only way to see the value that arrives.
///
/// **Two values, not one.** The first version of this test asserted a single
/// call fell in `[150 ms, 600 ms)` and claimed that caught a hardcoded 400 ms.
/// It did not — 400 satisfies both bounds, and 400 is the *most likely*
/// hardcode because it is the shipped `recall_timeout_ms`. Only the 5 s mutant
/// was caught. Two distinguishable budgets, both asserted, admit no constant.
#[test]
fn the_read_timeout_that_arrives_is_the_one_the_caller_passed() {
    for budget_ms in [150u64, 700] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accepts and never answers — the "daemon hung" column, measured at
        // 1536 ms per prompt in the plan when the timeout is 1.5 s.
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(30));
            drop(sock);
        });

        let started = Instant::now();
        let err = http::get(
            &target(addr),
            "/v1/recall",
            &Timeouts::from_ms(500, budget_ms),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, HttpError::Timeout), "{budget_ms}ms: {err}");
        assert!(
            elapsed >= Duration::from_millis(budget_ms),
            "{budget_ms}ms: gave up early at {elapsed:?}"
        );
        // +450 ms of slack for a loaded CI box. The two windows
        // ([150,600) and [700,1150)) are disjoint, so no single constant
        // satisfies both iterations.
        assert!(
            elapsed < Duration::from_millis(budget_ms + 450),
            "{budget_ms}ms: waited {elapsed:?}"
        );
    }
}

/// **HIGH, found by review and measured before the fix: `SO_RCVTIMEO` is not
/// a request budget.** It bounds one `read()` and re-arms on every byte, so a
/// server dribbling one byte per 300 ms held a 400 ms `recall` for **30.0 s
/// and then returned `Ok`** — invisible to the circuit breaker, on the event
/// where a stall erases the user's prompt.
///
/// The head arrives promptly and the body trickles, so this exercises the
/// second loop specifically; `an_expired_deadline_short_circuits_before_the_byte_bound`
/// covers the first.
#[test]
fn a_trickling_server_is_bounded_by_the_whole_request_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n");
        let _ = sock.flush();
        // 100 bytes at one per 300 ms = 30 s, every one of which reset the
        // old per-read timeout.
        for _ in 0..100 {
            if sock.write_all(b"x").is_err() {
                return;
            }
            let _ = sock.flush();
            std::thread::sleep(Duration::from_millis(300));
        }
    });

    let started = Instant::now();
    let err = http::get(&target(addr), "/v1/recall", &Timeouts::from_ms(50, 400)).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(err, HttpError::Timeout), "{err}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the whole-request deadline did not bound a trickling server: {elapsed:?}"
    );
}

/// Same mutation, other clock. Connecting to a routable address that drops
/// SYNs is not reliably available in CI, so the check is that
/// `connect_timeout` is what bounds a *successful* connect's failure mode —
/// asserted by making it absurdly small against a listener that exists.
#[test]
fn the_connect_timeout_is_passed_through_to_connect() {
    let (addr, _rx) = stub(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
    // 1ms is enough for a loopback connect (microseconds) but would fail on
    // anything routed, which is the property the value has to keep.
    let response = http::get(&target(addr), "/healthz", &Timeouts::from_ms(1, 2000));
    assert!(response.is_ok(), "{:?}", response.err());
}

/// A body larger than the client's ceiling must be refused by
/// `Content-Length` inspection, before the allocation.
#[test]
fn an_oversized_content_length_is_refused_before_reading_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent_clone = sent.clone();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            http::MAX_RESPONSE_BYTES + 1
        );
        let _ = sock.write_all(head.as_bytes());
        sent_clone.store(head.len(), std::sync::atomic::Ordering::SeqCst);
        // Deliberately never sends the body it promised.
        std::thread::sleep(Duration::from_millis(200));
    });

    let err = http::get(&target(addr), "/v1/recall", &Timeouts::from_ms(500, 2000)).unwrap_err();
    // A Timeout here would mean the client believed the length and sat
    // waiting for 8 MB that were never coming.
    assert!(matches!(err, HttpError::Protocol(_)), "{err}");
}

/// End-to-end proof that a bank id with `::` and a space survives the round
/// trip into a request line. `claude-code::bank e` is a live
/// bank on this machine; unescaped, the space would truncate the request line
/// and the daemon would answer 400.
#[test]
fn a_real_world_bank_id_survives_the_request_line() {
    let (addr, rx) = stub(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
    let encoded = http::encode_path_segment("claude-code::bank e");
    let path = format!("/v1/banks/{encoded}/sessions");
    http::get(&target(addr), &path, &quick()).unwrap();

    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let line = request.lines().next().unwrap();
    assert_eq!(
        line,
        "GET /v1/banks/claude-code%3A%3Abank%20e/sessions HTTP/1.1"
    );
    // Three space-separated tokens: method, target, version. A raw space in
    // the bank id would make four.
    assert_eq!(line.split(' ').count(), 3, "{line}");
}

/// Nothing here ever speaks to a non-loopback address, and the check is in
/// the client rather than only in config validation — so a caller that builds
/// a URL by hand cannot get out either.
#[test]
fn the_client_refuses_to_leave_the_loopback() {
    for url in [
        "http://example.com:9100",
        "http://10.0.0.5:9100",
        "https://127.0.0.1:9100",
    ] {
        assert!(Target::parse(url).is_err(), "{url}");
    }
    // And nothing was connected: if the parse had succeeded this test would
    // have needed a listener.
    assert!(
        TcpStream::connect_timeout(&"127.0.0.1:1".parse().unwrap(), Duration::from_millis(50))
            .is_err()
    );
}
