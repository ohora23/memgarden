//! Loopback HTTP/1.1, hand-rolled over `std::net`.
//!
//! Why not `reqwest`, which is already in the workspace: it pulls `tokio`, and
//! spinning up a multi-threaded async runtime inside a process whose entire
//! budget is 0.34 ms *is* the cost. This module is the whole client — one
//! connection, one request, one response, no keep-alive, no redirects, no TLS,
//! no chunked decoding.
//!
//! That list of "no"s is only safe because of what is on the other end:
//! `memgardend`, on loopback, which we also own. Three consequences are
//! load-bearing rather than incidental:
//!
//! * **No TLS.** The daemon binds `127.0.0.1` only. `daemon_url` is validated
//!   to `http://` in `memgarden-core`, and [`Target::parse`] additionally
//!   refuses any host that is not loopback — so a config typo can never make
//!   a hook talk to the network in cleartext.
//! * **`Host` is mandatory and must be loopback.** The daemon's `check_host`
//!   403s anything else (`middleware.rs:34-46`), port stripped, so
//!   `127.0.0.1:9100` is fine and `evil.com` is not.
//! * **Chunked transfer encoding is a failure, not a format.** axum serializes
//!   `Json` to bytes and sets `Content-Length`; it never chunks a response we
//!   ask for. So a chunked reply means we are not talking to the daemon we
//!   think we are, and mis-parsing it silently would be worse than failing.
//!   [`parse_head`] rejects any `Transfer-Encoding` outright.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Ceiling on the status line plus headers, in **bytes**. Time is bounded
/// separately, by the whole-request deadline in [`read_response`] — this one
/// only stops a server that never sends `\r\n\r\n` from growing the buffer
/// until the hook is OOM-killed (which would exit 137, not 2, but is still a
/// failure we can prevent).
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Ceiling on a response body. The largest thing we ever read is a recall
/// injection (~1.5 KB measured) or a session row; 8 MB is four orders of
/// magnitude of headroom and still bounds a `Content-Length` that lies.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The response header carrying the daemon's identity token
/// (`memgardend::token::TOKEN_HEADER`, mirrored rather than imported —
/// `memgardend` is not in this crate's dependency budget).
pub const TOKEN_HEADER: &str = "x-memgarden-token";

#[derive(Debug)]
pub enum HttpError {
    /// `daemon_url` is not a loopback `http://host:port`. A config fault, not
    /// a transport one — the caller must not count it as a transport failure
    /// and open the circuit breaker over it.
    Url(String),
    /// `<data>/daemon.token` could not be read, so we cannot tell `memgardend`
    /// apart from anything else listening on the port.
    ///
    /// Deliberately **not** a `Url` variant: a caller that treated it as a
    /// config fault would move no counter, the breaker would never open, and
    /// every prompt for the rest of the session would pay a full round trip to
    /// learn the same thing. It is transport-class.
    Token(String),
    /// `connect()` failed. ECONNREFUSED (daemon down) lands here.
    Connect(std::io::Error),
    /// A socket timeout elapsed. Split out from `Io` because it is the one
    /// failure whose *duration* the caller budgets for.
    Timeout,
    Io(std::io::Error),
    /// A well-formed connection carrying something that is not a response we
    /// are willing to parse.
    Protocol(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Url(m) => write!(f, "bad daemon url: {m}"),
            HttpError::Token(m) => write!(f, "daemon token unavailable: {m}"),
            HttpError::Connect(e) => write!(f, "connect failed: {e}"),
            HttpError::Timeout => write!(f, "timed out"),
            HttpError::Io(e) => write!(f, "io error: {e}"),
            HttpError::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl HttpError {
    fn from_io(e: std::io::Error) -> HttpError {
        // A socket read/write timeout surfaces as WouldBlock on unix and
        // TimedOut on windows; both mean "the deadline the caller set elapsed".
        match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HttpError::Timeout,
            _ => HttpError::Io(e),
        }
    }
}

/// The two clocks a hook budgets separately: how long we will wait to *reach*
/// the daemon, and how long we will wait for it to *answer*. Recall and retain
/// use very different values for the second (400 ms vs 5 s) because the work
/// behind the two endpoints differs by four orders of magnitude.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub io: Duration,
}

impl Timeouts {
    pub fn from_ms(connect_ms: u64, io_ms: u64) -> Timeouts {
        Timeouts {
            connect: Duration::from_millis(connect_ms),
            io: Duration::from_millis(io_ms),
        }
    }
}

/// A resolved daemon address plus the exact `Host` header value to send.
#[derive(Debug, Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub host_header: String,
    /// The token every response from this target must carry, when the caller
    /// has one. `None` skips the check — which is what the in-process
    /// transport tests and the benchmark stub want, and which is safe because
    /// the only production constructor is `cmd::target`, and that one fails
    /// rather than returning `None`.
    ///
    /// It is **never sent**. See `memgardend::token` for why the secret
    /// travels one way: a request that carried it would hand it to the
    /// impostor this check exists to catch, which could then echo it back.
    pub token: Option<String>,
}

impl Target {
    /// Parses `http://<loopback-host>[:port]`, with an optional trailing `/`.
    ///
    /// Resolution is done here, once, and `localhost` is mapped to
    /// `127.0.0.1` **by table rather than by resolver**: `getaddrinfo` reads
    /// `/etc/nsswitch.conf` and can consult a network service, which is not a
    /// dependency a 0.34 ms process should acquire for a name whose answer is
    /// fixed.
    pub fn parse(base_url: &str) -> Result<Target, HttpError> {
        let rest = base_url
            .strip_prefix("http://")
            .ok_or_else(|| HttpError::Url(format!("must start with http://: {base_url}")))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if !path.is_empty() && path != "/" {
            // Silently dropping a path prefix would send every request to the
            // wrong place with no error anywhere.
            return Err(HttpError::Url(format!(
                "daemon url must have no path component: {base_url}"
            )));
        }

        let (host, port) = split_authority(authority)?;
        let ip = match host {
            "127.0.0.1" | "localhost" => std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            "::1" => std::net::IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => {
                return Err(HttpError::Url(format!(
                    "host must be loopback (127.0.0.1, localhost, ::1), got {other}"
                )));
            }
        };
        Ok(Target {
            addr: SocketAddr::new(ip, port),
            // Sent verbatim, port included. `check_host` strips the port and
            // matches the host part, so all three spellings pass.
            host_header: authority.to_string(),
            token: None,
        })
    }

    /// [`Target::parse`] plus the token every response must carry.
    pub fn parse_verified(base_url: &str, token: String) -> Result<Target, HttpError> {
        Ok(Target {
            token: Some(token),
            ..Target::parse(base_url)?
        })
    }
}

/// Compares two secrets without leaking their common prefix through timing.
///
/// Four lines rather than `subtle`: this crate's dependency closure is
/// CI-enforced, and the comparison is a fixed-length hex string. The length
/// itself is not secret, so an early return on a length mismatch is fine.
fn tokens_match(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn split_authority(authority: &str) -> Result<(&str, u16), HttpError> {
    // Bracketed IPv6 first — the address contains the delimiter.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| HttpError::Url(format!("unterminated [: {authority}")))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None => 80,
        };
        return Ok((host, port));
    }
    // More than one `:` in an unbracketed authority is a bare IPv6 address
    // (`::1:9100`). `rsplit_once` would read the last group as a port and the
    // rest as a host, which then passes our allowlist by accident and 403s at
    // the daemon. Refuse it here, where the message can say why.
    if authority.matches(':').count() > 1 {
        return Err(HttpError::Url(format!(
            "ipv6 authority must be bracketed, e.g. http://[::1]:9100 — got {authority}"
        )));
    }
    match authority.rsplit_once(':') {
        Some((host, p)) => Ok((host, parse_port(p)?)),
        None => Ok((authority, 80)),
    }
}

fn parse_port(raw: &str) -> Result<u16, HttpError> {
    raw.parse()
        .map_err(|_| HttpError::Url(format!("bad port: {raw}")))
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub fn post(
    target: &Target,
    path: &str,
    body: &[u8],
    timeouts: &Timeouts,
) -> Result<Response, HttpError> {
    request(target, "POST", path, Some(body), timeouts)
}

pub fn get(target: &Target, path: &str, timeouts: &Timeouts) -> Result<Response, HttpError> {
    request(target, "GET", path, None, timeouts)
}

fn request(
    target: &Target,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeouts: &Timeouts,
) -> Result<Response, HttpError> {
    // The one place every request routes through, so the escaping rule is
    // enforced here rather than in each subcommand. **Bank ids reach the path
    // from untrusted stdin** and do so today: `bank::derive` takes the
    // payload's `cwd`, so a stdin-controlled string is in the request line of
    // every `session-start`. `encode_path_segment` exists and is correct, but
    // nothing *enforced* it, and a raw space or CR in a path is request
    // splitting, not a 400.
    //
    // Session ids are **not** in this set, despite what C2a's version of this
    // comment predicted: C2b puts `session_id` only inside a JSON body, where
    // serde escapes it. C4b's `GET /v1/retain/{job_id}` is the next path
    // segment that comes from outside.
    if path.is_empty() || !path.starts_with('/') || path.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(HttpError::Url(format!(
            "path must be an escaped absolute path: {path:?}"
        )));
    }

    let stream =
        TcpStream::connect_timeout(&target.addr, timeouts.connect).map_err(HttpError::Connect)?;
    // Both directions get the caller's budget. Without the write timeout a
    // daemon whose receive buffer is full would block a `Stop` hook forever on
    // a 9 MB initial retain.
    //
    // NOTE: these are `SO_RCVTIMEO`/`SO_SNDTIMEO`, which bound **one** syscall
    // and re-arm on every byte. They are not the request budget — see
    // `read_response`'s deadline.
    stream
        .set_read_timeout(Some(timeouts.io))
        .map_err(HttpError::from_io)?;
    stream
        .set_write_timeout(Some(timeouts.io))
        .map_err(HttpError::from_io)?;
    // A JSON body is one small write followed by a read; Nagle would hold it
    // for an ACK that is not coming until the daemon has answered.
    stream.set_nodelay(true).map_err(HttpError::from_io)?;

    // Built into one buffer and written once: two `write_all`s are two
    // syscalls and, with `TCP_NODELAY`, two packets.
    let mut req = Vec::with_capacity(256 + body.map_or(0, <[u8]>::len));
    req.extend_from_slice(method.as_bytes());
    req.push(b' ');
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(target.host_header.as_bytes());
    // `Connection: close` makes the daemon hang up after one response, which
    // is what we want: the process is about to exit anyway, and a pooled
    // connection has nobody to pool it for.
    req.extend_from_slice(b"\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.extend_from_slice(b"Content-Type: application/json\r\nContent-Length: ");
        req.extend_from_slice(body.len().to_string().as_bytes());
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        req.extend_from_slice(body);
    }
    // `&TcpStream` implements both `Read` and `Write`, which is what lets the
    // reader and the timeout-setter below borrow the socket at the same time.
    (&mut &stream).write_all(&req).map_err(HttpError::from_io)?;
    (&mut &stream).flush().map_err(HttpError::from_io)?;

    read_response(
        &mut &stream,
        &|left| stream.set_read_timeout(Some(left)),
        Instant::now() + timeouts.io,
        target.token.as_deref(),
    )
}

/// Reads until the header terminator, then **exactly** `Content-Length` more
/// bytes, and gives up at `deadline` however the bytes are paced.
///
/// **`SO_RCVTIMEO` is not a request budget.** It bounds one `read()` and
/// re-arms on every byte that arrives, so a server dribbling one byte per
/// 300 ms held a 400 ms-budgeted `recall` for a measured **30 s and then
/// returned `Ok`** — invisible to the circuit breaker, on the event where a
/// stall erases the user's prompt. `MAX_HEAD_BYTES` did not help: it bounds
/// bytes, not time. So the socket option stays (it bounds a single blocked
/// syscall) and `deadline` is re-armed onto it before every read, which makes
/// the *whole request* cost at most `timeouts.io`.
///
/// Deliberately still not `read_to_end`: that would be shorter and wrong — it
/// waits for the peer's FIN even after the whole body has arrived, so a daemon
/// that ignored `Connection: close` would cost every hook its full budget
/// while looking perfectly healthy.
fn read_response(
    stream: &mut impl Read,
    rearm: &dyn Fn(Duration) -> std::io::Result<()>,
    deadline: Instant,
    expect_token: Option<&str>,
) -> Result<Response, HttpError> {
    // `Some(ZERO)` is rejected by `set_read_timeout`, and a zero budget has
    // already elapsed anyway.
    let tick = |rearm: &dyn Fn(Duration) -> std::io::Result<()>| -> Result<(), HttpError> {
        match deadline.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => rearm(left).map_err(HttpError::from_io),
            _ => Err(HttpError::Timeout),
        }
    };

    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HttpError::Protocol(format!(
                "response head exceeds {MAX_HEAD_BYTES} bytes"
            )));
        }
        tick(rearm)?;
        let n = stream.read(&mut chunk).map_err(HttpError::from_io)?;
        if n == 0 {
            return Err(HttpError::Protocol(
                "connection closed before the response head was complete".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    // Checked on the **head**, before a single body byte is read: if this is
    // not memgardend, the cheapest thing to do with its body is not read it.
    let (status, content_length) = parse_head(&buf[..head_end], expect_token)?;
    let mut body = buf.split_off(head_end + 4);
    body.reserve(content_length.saturating_sub(body.len()));
    while body.len() < content_length {
        let want = (content_length - body.len()).min(chunk.len());
        tick(rearm)?;
        let n = stream
            .read(&mut chunk[..want])
            .map_err(HttpError::from_io)?;
        if n == 0 {
            return Err(HttpError::Protocol(format!(
                "connection closed after {} of {content_length} body bytes",
                body.len()
            )));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Response { status, body })
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Returns `(status, content_length)`, or an error for anything we refuse to
/// guess at — including a response that does not prove it came from
/// `memgardend` when the caller gave us a token to check against.
fn parse_head(head: &[u8], expect_token: Option<&str>) -> Result<(u16, usize), HttpError> {
    let head = std::str::from_utf8(head)
        .map_err(|_| HttpError::Protocol("response head is not utf-8".to_string()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol("empty response".to_string()))?;
    if !status_line.starts_with("HTTP/1.1 ") && !status_line.starts_with("HTTP/1.0 ") {
        return Err(HttpError::Protocol(format!(
            "not an HTTP/1.x response: {status_line:?}"
        )));
    }
    let status: u16 = status_line
        .get(9..12)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::Protocol(format!("no status code in {status_line:?}")))?;

    let mut content_length = None;
    let mut presented_token = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case(TOKEN_HEADER) {
            presented_token = Some(value);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // See the module comment: axum sets Content-Length and never
            // chunks, so this is evidence of a different server, a proxy, or a
            // bug. Failing is the caller's fail-open path; mis-parsing would
            // put a chunk-size line into the model's context.
            return Err(HttpError::Protocol(format!(
                "transfer-encoding is not supported (got {value:?}); the daemon always \
                 sets content-length"
            )));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let n: usize = value
                .parse()
                .map_err(|_| HttpError::Protocol(format!("bad content-length {value:?}")))?;
            if n > MAX_RESPONSE_BYTES {
                return Err(HttpError::Protocol(format!(
                    "content-length {n} exceeds {MAX_RESPONSE_BYTES}"
                )));
            }
            // Two disagreeing lengths is request-smuggling shaped, and this
            // module's whole argument is that it refuses ambiguity rather than
            // picking a reading. Two *agreeing* ones are merely redundant.
            if content_length.is_some_and(|prev| prev != n) {
                return Err(HttpError::Protocol(
                    "conflicting content-length headers".to_string(),
                ));
            }
            content_length = Some(n);
        }
    }
    // **Is this memgardend?** `Target::parse` and the daemon's `check_host`
    // both only answer "is this loopback". 9100 is unprivileged and nothing
    // sets `SO_REUSEPORT`, so any local uid can hold it across a restart of
    // ours and answer 200 with an `injected_text` that reaches the model
    // verbatim — demonstrated against C3, which is the first PR where a
    // daemon-supplied byte reaches stdout at all. A response that cannot
    // produce the token from our 0600 `<data>/daemon.token` is not a bad
    // response, it is a different server.
    if let Some(expected) = expect_token
        && !presented_token.is_some_and(|p| tokens_match(expected, p))
    {
        return Err(HttpError::Protocol(
            "response did not carry this daemon's identity token; refusing to read it".to_string(),
        ));
    }
    // No length and no chunking leaves "read until close" as the only reading,
    // which is exactly the ambiguity this client refuses to have.
    let content_length = content_length
        .ok_or_else(|| HttpError::Protocol("response has no content-length header".to_string()))?;
    Ok((status, content_length))
}

/// Percent-encodes one URL **path segment**, escaping everything outside RFC
/// 3986's unreserved set.
///
/// Bank ids go in the path and routinely contain characters that must not be
/// there raw: the live ids are `claude-code::bank-b` and
/// `claude-code::bank e` — a `::` and a space (plan §Binding
/// decisions #4).
pub fn encode_path_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loopback_urls_and_refuses_everything_else() {
        let t = Target::parse("http://127.0.0.1:9100").unwrap();
        assert_eq!(t.addr, "127.0.0.1:9100".parse::<SocketAddr>().unwrap());
        assert_eq!(t.host_header, "127.0.0.1:9100");

        // localhost resolves by table, not by getaddrinfo.
        let t = Target::parse("http://localhost:9100/").unwrap();
        assert_eq!(t.addr, "127.0.0.1:9100".parse::<SocketAddr>().unwrap());
        assert_eq!(t.host_header, "localhost:9100");

        let t = Target::parse("http://[::1]:9100").unwrap();
        assert_eq!(t.addr, "[::1]:9100".parse::<SocketAddr>().unwrap());

        for bad in [
            "https://127.0.0.1:9100",   // no TLS stack is linked
            "http://example.com:9100",  // not loopback
            "http://127.0.0.1:9100/v1", // path prefix would misroute
            "http://127.0.0.1:999999",  // not a port
            "127.0.0.1:9100",           // no scheme
            // Unbracketed ipv6: `rsplit_once(':')` would read `9100` as the
            // port and `::1` as the host, pass the allowlist by accident, and
            // 403 at the daemon with no useful message.
            "http://::1:9100",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad} must be refused");
        }
    }

    /// The compare must not accept a prefix, a near miss or a different
    /// length. It is `fold`-based rather than `==` so the loop does not exit
    /// on the first differing byte and leak the common prefix through timing.
    #[test]
    fn the_token_compare_accepts_only_an_exact_match() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(tokens_match(token, token));
        assert!(!tokens_match(token, ""));
        assert!(!tokens_match(token, "0123456789abcdef"));
        assert!(!tokens_match(token, "0123456789abcdef0123456789abcde0"));
        assert!(!tokens_match(token, &format!("{token}x")));
        // Case matters: the daemon writes lowercase hex and nothing normalizes.
        assert!(!tokens_match(token, &token.to_uppercase()));
    }

    /// A response that cannot produce the token is refused **on the head**,
    /// before its body is read — an impostor's bytes are not worth reading.
    #[test]
    fn a_response_without_this_daemons_token_is_refused() {
        let token = "0123456789abcdef";
        let with = format!("HTTP/1.1 200 OK\r\nx-memgarden-token: {token}\r\ncontent-length: 2");
        assert_eq!(parse_head(with.as_bytes(), Some(token)).unwrap(), (200, 2));
        // Header name matching is case-insensitive on the wire, like the rest.
        let upper = format!("HTTP/1.1 200 OK\r\nX-Memgarden-Token: {token}\r\ncontent-length: 2");
        assert_eq!(parse_head(upper.as_bytes(), Some(token)).unwrap(), (200, 2));

        for hostile in [
            "HTTP/1.1 200 OK\r\ncontent-length: 2",
            "HTTP/1.1 200 OK\r\nx-memgarden-token: \r\ncontent-length: 2",
            "HTTP/1.1 200 OK\r\nx-memgarden-token: 0123456789abcdee\r\ncontent-length: 2",
        ] {
            let err = parse_head(hostile.as_bytes(), Some(token)).unwrap_err();
            assert!(
                matches!(err, HttpError::Protocol(_)),
                "{hostile:?} -> {err}"
            );
        }
        // …and with no token to check against, the header is simply ignored:
        // that is what the in-process transport tests and the bench stub need.
        assert_eq!(
            parse_head(b"HTTP/1.1 200 OK\r\ncontent-length: 2", None).unwrap(),
            (200, 2)
        );
    }

    #[test]
    fn encodes_the_two_live_bank_ids() {
        assert_eq!(
            encode_path_segment("claude-code::bank-b"),
            "claude-code%3A%3Abank-b"
        );
        assert_eq!(
            encode_path_segment("claude-code::bank e"),
            "claude-code%3A%3Abank%20e"
        );
        // Unreserved characters survive; a slash would otherwise change the route.
        assert_eq!(encode_path_segment("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        // Multi-byte input is encoded per byte, not per char.
        assert_eq!(encode_path_segment("한"), "%ED%95%9C");
    }

    fn head(raw: &str) -> Result<(u16, usize), HttpError> {
        parse_head(raw.as_bytes(), None)
    }

    #[test]
    fn head_parsing_accepts_the_daemons_shape() {
        let (status, len) = head("HTTP/1.1 202 Accepted\r\ncontent-length: 17").unwrap();
        assert_eq!((status, len), (202, 17));
        // Header names are case-insensitive on the wire.
        let (status, len) = head("HTTP/1.1 404 Not Found\r\nContent-Length: 0").unwrap();
        assert_eq!((status, len), (404, 0));
    }

    #[test]
    fn head_parsing_refuses_chunked_and_lengthless_replies() {
        let err = head("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked").unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err}");
        let err = head("HTTP/1.1 200 OK\r\ncontent-type: application/json").unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err}");
        // A length that lies about a gigabyte must not be allocated.
        let err = head("HTTP/1.1 200 OK\r\ncontent-length: 1073741824").unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err}");
        // Two disagreeing lengths: pick neither.
        let err = head("HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-length: 9").unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err}");
        // Two agreeing ones are redundant, not ambiguous.
        assert_eq!(
            head("HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-length: 5").unwrap(),
            (200, 5)
        );
        for bad in [
            "ICY 200 OK\r\ncontent-length: 0",
            "HTTP/1.1 \r\ncontent-length: 0",
            "HTTP/1.1 200 OK\r\ncontent-length: many",
        ] {
            assert!(head(bad).is_err(), "{bad} must be refused");
        }
    }

    /// An in-memory reader never blocks, so the deadline is generous and the
    /// re-arm is a no-op. The deadline's real behaviour needs a socket and
    /// lives in `tests/http_transport.rs`.
    fn read_for_test(stream: &mut impl Read) -> Result<Response, HttpError> {
        read_response(
            stream,
            &|_| Ok(()),
            Instant::now() + Duration::from_secs(60),
            None,
        )
    }

    /// The body is taken from `Content-Length`, not from "everything until
    /// EOF": a server that appends trailing junk must not have it handed to
    /// the caller.
    #[test]
    fn body_is_bounded_by_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhelloTRAILING JUNK";
        let response = read_for_test(&mut &raw[..]).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");
        assert!(response.is_success());
    }

    #[test]
    fn a_truncated_body_is_an_error_not_a_short_read() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 99\r\n\r\nhello";
        assert!(read_for_test(&mut &raw[..]).is_err());
    }

    #[test]
    fn an_endless_headerless_stream_does_not_grow_without_bound() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let err = read_for_test(&mut Endless).unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err}");
    }

    /// The byte ceiling and the time ceiling are independent, and this is the
    /// half that proves the deadline works without a socket: an endless
    /// reader that never completes a head, against an already-expired
    /// deadline, is `Timeout` rather than the byte-bound `Protocol`.
    #[test]
    fn an_expired_deadline_short_circuits_before_the_byte_bound() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let past = Instant::now() - Duration::from_secs(1);
        let err = read_response(&mut Endless, &|_| Ok(()), past, None).unwrap_err();
        assert!(matches!(err, HttpError::Timeout), "{err}");
    }

    /// The path guard is at the choke point, so every future subcommand
    /// inherits it. Asserted without a listener: it must fail before connect.
    #[test]
    fn a_path_with_control_characters_never_reaches_the_socket() {
        // Port 1 is not listening; a reachable path would give Connect, not Url.
        let target = Target::parse("http://127.0.0.1:1").unwrap();
        let t = Timeouts::from_ms(50, 50);
        for bad in [
            "",
            "v1/recall",                         // not absolute
            "/v1/banks/a b/sessions",            // raw space = request splitting
            "/v1/banks/a\r\nX-Evil: 1/sessions", // CRLF injection
            "/v1/banks/a\u{7f}/sessions",        // DEL
            "/v1/banks/a\u{0}/sessions",         // NUL
        ] {
            let err = get(&target, bad, &t).unwrap_err();
            assert!(matches!(err, HttpError::Url(_)), "{bad:?} -> {err}");
        }
        // The escaped form of the same bank id is accepted by the guard and
        // then fails at connect, which is what "reached the socket" looks like.
        let ok = format!("/v1/banks/{}/sessions", encode_path_segment("a b"));
        assert!(matches!(get(&target, &ok, &t), Err(HttpError::Connect(_))));
    }
}
