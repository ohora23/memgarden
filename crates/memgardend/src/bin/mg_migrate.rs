//! `mg-migrate` — the legacy migration tool (MG-1, MG-2).
//!
//! ```text
//! mg_migrate snapshot --out <dir>
//! ```
//!
//! `import` (D2) and `verify` (D3) land next; the `match` below is where they
//! wire in. **D1 writes no database row anywhere** — it reads legacy over
//! HTTP, writes files, and asserts.
//!
//! A binary in `memgardend` rather than a new crate: the importer that follows
//! needs `rusqlite`, `fastembed` and this crate's whole library surface, which
//! is precisely the crate it belongs in. `#[tokio::main]` because `reqwest` is
//! declared in this workspace without its `blocking` feature (`Cargo.toml`,
//! B2) and D2 will await `embed_task::drain_once`.

use std::path::{Path, PathBuf};

const USAGE: &str = "\
usage:
  mg_migrate snapshot --out <dir>

`snapshot` issues five read-only GETs against the legacy daemon on
http://127.0.0.1:9077, writes each bank's transfer archive verbatim beside its
unpacked form, records /stats, /documents and the invalidated-fact census in
stats.json, and refuses — non-zero, naming the property — if any of the
integrity identities measured true today has stopped holding.

It never issues anything but GET, and it writes no database row.";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Positional args and one flag, no clap: one subcommand with fixed arity
    // does not need an argument parser, and this mirrors `recall_bench.rs:811`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = |i: usize| PathBuf::from(&args[i]);
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["snapshot", "--out", _] => snapshot(&out(2)).await,
        _ => {
            eprintln!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Exit 1, distinct from usage's 2. D3 adds a third code for a Tier-2
        // review stop, so keeping 2 reserved for "you called it wrong" starts
        // here.
        Err(e) => {
            eprintln!("mg_migrate: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn snapshot(out: &Path) -> memgardend::migrate::Result<()> {
    println!("snapshot -> {}", out.display());
    let lines = memgardend::migrate::snapshot::run(out).await?;
    for line in &lines {
        println!("{line}");
    }
    // The operator's own independent check is `sha256sum -c SHA256SUMS`; this
    // is ours, run immediately so a snapshot that cannot verify itself never
    // reaches the runbook's next line.
    memgardend::migrate::snapshot::verify_sha256sums(out)?;
    println!("SHA256SUMS written and verified");
    Ok(())
}
