//! Interleaved-paired hook benchmark (plan §Measurement design).
//!
//! # Why interleaved-paired, and never two separate runs
//!
//! Absolute cross-session comparison is invalid on this machine: re-benching
//! an identical commit returned **+1.5 ms on identical bits**. So every number
//! a Phase C PR quotes comes from one driver process that alternates
//! `A,B,A,B…`, where **B is `memgarden hook noop`** — the same binary, the
//! same dynamic-link and page-cache state, parsing argv and exiting.
//!
//! * **Arm B** is the binary's fixed cost. Budget: paired p50 **<= 1.5 ms**.
//!   Unlike the daemon, a hook pays for its binary on every `execve`, so
//!   size and relocation count are inside this number.
//! * **Arm A** is the subcommand under test.
//! * **A_i - B_i** is the subcommand's own work, and it survives a noisy box
//!   because noise moves both arms.
//!
//! C2a ships `noop` only, so the default run is A = B = `hook noop`: a null
//! experiment whose paired delta must sit at ~0. That is the right first
//! result for a measuring instrument — a harness that cannot measure "no
//! difference" cannot be trusted to measure a difference.
//!
//! # What is *not* in these numbers
//!
//! `hook_overhead` is defined as `execve` -> exit **with the daemon's service
//! time excluded** (R7: "훅**당** 오버헤드 <10ms"). That is why the stub
//! daemon replies from a pre-serialized buffer: the transport is real, the
//! daemon's work is not counted twice. `--real <url>` switches to a live
//! `memgardend` for **Gate C**, which is an AC-2 *recall-clause* number
//! (p95 <= 70 ms end to end), not a hook-overhead one. They are labelled
//! separately so a daemon regression never reads as a hook regression.
//!
//! # Usage
//!
//! ```text
//! cargo build --release -p memgarden-cli --bins
//! ./target/release/hook_bench --n 300 --warmup 20
//! ./target/release/hook_bench --arm-a "hook recall" --stdin-a payload.json
//! ./target/release/hook_bench --real http://127.0.0.1:9100   # Gate C
//! ```

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use memgarden_cli::http::{self, Target, Timeouts};

/// Arm B's budget (plan §Measurement design). Printed alongside the result so
/// the pass/fail is in the output rather than in the reader's memory.
const ARM_B_BUDGET_MS: f64 = 1.5;

struct Args {
    bin: String,
    n: usize,
    warmup: usize,
    arm_a: Vec<String>,
    arm_b: Vec<String>,
    stdin_a: Vec<u8>,
    /// Defaults to `--stdin-a`, not to empty. The null experiment is only a
    /// control if the two arms are identical *including* their stdin: arm B
    /// reading nothing while arm A reads a 4 KB payload charges the delta for
    /// a pipe write that has nothing to do with the subcommand. Pass
    /// `--stdin-b ""`-style divergence only deliberately.
    stdin_b: Option<Vec<u8>>,
    real: Option<String>,
    transport_n: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        // Same directory as this binary, which is how `cargo build --release
        // --bins` leaves them.
        bin: default_hook_bin(),
        n: 300,
        warmup: 20,
        arm_a: vec!["hook".into(), "noop".into()],
        arm_b: vec!["hook".into(), "noop".into()],
        stdin_a: Vec::new(),
        stdin_b: None,
        real: None,
        transport_n: 200,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].clone();
        let value = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{flag} needs a value"))
                .clone()
        };
        match flag.as_str() {
            "--bin" => args.bin = value(),
            "--n" => args.n = value().parse().expect("--n"),
            "--warmup" => args.warmup = value().parse().expect("--warmup"),
            "--arm-a" => args.arm_a = split_words(&value()),
            "--arm-b" => args.arm_b = split_words(&value()),
            "--stdin-a" => args.stdin_a = std::fs::read(value()).expect("--stdin-a"),
            "--stdin-b" => args.stdin_b = Some(std::fs::read(value()).expect("--stdin-b")),
            "--real" => args.real = Some(value()),
            "--transport-n" => args.transport_n = value().parse().expect("--transport-n"),
            other => panic!("unknown flag {other}"),
        }
        i += 2;
    }
    args
}

fn default_hook_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("memgarden")))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "memgarden".to_string())
}

fn split_words(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_string).collect()
}

/// A stub `memgardend`: accepts, reads the request, replies from a
/// pre-serialized buffer. Lives in this process so the bench has no external
/// dependency and can run in a sandbox.
///
/// // ponytail: one connection at a time, sequentially. The driver is
/// // single-threaded and the hook opens exactly one connection per
/// // invocation, so a thread per connection would buy nothing; spawn per
/// // accept if a concurrent arm is ever added.
fn spawn_stub() -> String {
    // A ~1.5 KB body, the measured size of a real recall response.
    let injected = "· ".repeat(600);
    let body = serde_json::json!({
        "injected_text": injected,
        "count": 8,
        "took_ms": 12,
    })
    .to_string();
    let reply = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(reply.as_bytes());
            let _ = sock.flush();
        }
    });
    format!("http://{addr}")
}

/// One `execve` -> exit, measured from the driver. Stdin is written and
/// closed; stdout and stderr are discarded so a subcommand that writes cannot
/// block on a full pipe.
fn run_once(bin: &str, args: &[String], stdin: &[u8], daemon_url: &str) -> Duration {
    let started = Instant::now();
    let mut child = Command::new(bin)
        .args(args)
        .env("MEMGARDEN_DAEMON_URL", daemon_url)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"));
    {
        let mut pipe = child.stdin.take().expect("stdin");
        let _ = pipe.write_all(stdin);
    }
    let status = child.wait().expect("wait");
    let elapsed = started.elapsed();
    // A hook that exits 2 erases the user's prompt. If the benchmark ever
    // sees one, the number is the least of the problems.
    assert_ne!(status.code(), Some(2), "hook exited 2: {args:?}");
    elapsed
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

struct Stats {
    p50: f64,
    p95: f64,
    p99: f64,
    min: f64,
}

fn stats(mut values: Vec<f64>) -> Stats {
    values.sort_by(f64::total_cmp);
    Stats {
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        p99: percentile(&values, 0.99),
        min: values.first().copied().unwrap_or(f64::NAN),
    }
}

fn row(label: &str, s: &Stats) {
    println!(
        "| {label} | {:.3} | {:.3} | {:.3} | {:.3} |",
        s.p50, s.p95, s.p99, s.min
    );
}

fn main() {
    let args = parse_args();
    let daemon_url = match &args.real {
        Some(url) => {
            println!("# Gate C mode: live daemon at {url}");
            println!(
                "# NOTE: these are AC-2 **recall-clause** numbers (daemon service time \
                 INCLUDED), not hook overhead.\n"
            );
            url.clone()
        }
        None => spawn_stub(),
    };

    println!("bin       : {}", args.bin);
    println!("arm A     : {:?}", args.arm_a);
    println!("arm B     : {:?} (paired baseline)", args.arm_b);
    println!("daemon    : {daemon_url}");
    println!(
        "N         : {} (+{} discarded warm-ups)",
        args.n, args.warmup
    );
    println!();

    let stdin_b = args.stdin_b.clone().unwrap_or_else(|| args.stdin_a.clone());

    for _ in 0..args.warmup {
        run_once(&args.bin, &args.arm_a, &args.stdin_a, &daemon_url);
        run_once(&args.bin, &args.arm_b, &stdin_b, &daemon_url);
    }

    let mut a_ms = Vec::with_capacity(args.n);
    let mut b_ms = Vec::with_capacity(args.n);
    let mut paired_ms = Vec::with_capacity(args.n);
    for _ in 0..args.n {
        // Strictly alternated inside one process: this is the whole design.
        let a = run_once(&args.bin, &args.arm_a, &args.stdin_a, &daemon_url).as_secs_f64() * 1e3;
        let b = run_once(&args.bin, &args.arm_b, &stdin_b, &daemon_url).as_secs_f64() * 1e3;
        a_ms.push(a);
        b_ms.push(b);
        paired_ms.push(a - b);
    }

    let a = stats(a_ms);
    let b = stats(b_ms);
    let paired = stats(paired_ms);

    println!("| arm | p50 ms | p95 ms | p99 ms | min ms |");
    println!("|---|---|---|---|---|");
    row(&format!("A `{}`", args.arm_a.join(" ")), &a);
    row(&format!("B `{}` (baseline)", args.arm_b.join(" ")), &b);
    row("paired A-B", &paired);
    println!();
    println!(
        "arm B p50 {:.3} ms vs budget {ARM_B_BUDGET_MS} ms -> {}",
        b.p50,
        if b.p50 <= ARM_B_BUDGET_MS {
            "PASS"
        } else {
            "OVER BUDGET"
        }
    );

    transport_probe(&daemon_url, args.transport_n);
}

/// In-process round trips through the same client the hook uses.
///
/// Explicitly **not** a hook-overhead number — there is no `execve` here. It
/// is reported because it separates "the transport is slow" from "the binary
/// is slow" when arm A moves, and because it is the only thing in C2a that
/// exercises `http.rs` against a socket at volume.
fn transport_probe(daemon_url: &str, n: usize) {
    let Ok(target) = Target::parse(daemon_url) else {
        println!("\n(transport probe skipped: {daemon_url} is not a loopback url)");
        return;
    };
    let timeouts = Timeouts::from_ms(50, 5000);
    let body = br#"{"query":"benchmark","maxTokens":1024}"#;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let started = Instant::now();
        let result = http::post(&target, "/v1/banks/bench/recall", body, &timeouts);
        let elapsed = started.elapsed().as_secs_f64() * 1e3;
        if let Err(e) = result {
            println!("\ntransport probe failed on iteration {i}: {e}");
            return;
        }
        // The first few include the listener's first accept; discard them the
        // same way the process arms discard warm-ups.
        if i >= 10 {
            samples.push(elapsed);
        }
    }
    // `samples.len()`, not `n - 10`: the latter underflows on `usize` for any
    // `--transport-n` under 10 and panics the harness in the one place a
    // measuring instrument must not.
    let reported = samples.len();
    let s = stats(samples);
    println!("\nin-process transport round trip (NOT hook overhead), N={reported}:");
    println!("| arm | p50 ms | p95 ms | p99 ms | min ms |");
    println!("|---|---|---|---|---|");
    row("http::post -> stub", &s);
}
