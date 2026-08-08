//! `mg-migrate` — the legacy migration tool (MG-1, MG-2).
//!
//! ```text
//! mg_migrate snapshot --out <dir>
//! mg_migrate import   --snapshot <dir> --db <path> [--replace] [--defer-embeddings]
//! mg_migrate verify   --snapshot <dir> --db <path> [--out <file>] [--sample N]
//!                     [--seed N] [--accept-tier2 <sha256>] [--dump-only]
//! ```
//!
//! **`snapshot` writes no database row** — it reads legacy over HTTP, writes
//! files, and asserts. **`import` issues no HTTP at all** — it reads the
//! directory `snapshot` froze, which is the whole point of freezing it
//! (§Binding decisions #2: the archive, not the daemon, is the migration
//! source). **`verify` writes nothing at all** and exits 0, 1 or 2.
//!
//! A binary in `memgardend` rather than a new crate: the importer needs
//! `rusqlite`, `fastembed` and this crate's whole library surface, which is
//! precisely the crate it belongs in. `#[tokio::main]` because `reqwest` is
//! declared in this workspace without its `blocking` feature (`Cargo.toml`,
//! B2) and `import` awaits `embed_task::drain_once`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memgarden_core::config::Config;
use memgardend::migrate::{self, import};

const USAGE: &str = "\
usage:
  mg_migrate snapshot --out <dir> [--drop-bank <bank-id>]...
  mg_migrate import   --snapshot <dir> --db <path> [--replace] [--defer-embeddings]

`snapshot` issues five read-only GETs against the legacy daemon on
http://127.0.0.1:9077, writes each bank's transfer archive verbatim beside its
unpacked form, records /stats, /documents and the invalidated-fact census in
stats.json, and refuses — non-zero, naming the property — if any of the
integrity identities measured true today has stopped holding.

It never issues anything but GET, and it writes no database row.

  --drop-bank <id>    do not migrate this bank, and assert it is empty.
                      Repeatable; defaults to none. Naming a bank is a claim
                      that it holds nothing — the run fails if it does not,
                      and `verify` re-checks the claim from the frozen stats.
                      An unnamed empty bank is snapshotted anyway and skipped
                      at import, so passing none loses nothing.

`import` reads that directory and writes the banks into a MemGarden database:
documents, facts, tags, entities, observations with their provenance, the
authored causal links, and temporal and semantic links re-derived by our own
rules. It refuses a non-empty bank without --replace, and it refuses to open
the database a live daemon is holding.

  --replace           purge each migrated bank first: nodes, documents,
                      entities, mental models, consolidation runs and
                      sessions. retain_jobs rows are spared.
  --defer-embeddings  leave the FACT embedding backlog for the restarted
                      daemon to drain, which is also what writes the semantic
                      links. Observations are embedded either way — the store
                      takes their vector by value. Shortens the maintenance
                      window; `verify` will report the pending nodes until the
                      daemon has caught up.

`verify` is the AC-3 instrument. It reads three oracles — legacy's frozen
/stats, the frozen archive, and the database — and reports three tiers:
equality for everything that can be equal, recomputed adjacency reported
against a measured band, and the one type neither system stores. It writes
nothing and is safe against a live database.

  --out <file>        write the JSON report as well as the human table
  --sample N          content-diff N records, stratified by bank (default 50)
  --seed N            the sample's seed, so the run reproduces (default 1)
  --accept-tier2 <h>  acknowledge one specific Tier-2 result and downgrade its
                      exit 2 to 0. `h` is the report's own acceptance hash,
                      which the report prints; a stale hash does nothing.
  --dump-only         no comparison — emit what the database holds. This is the
                      runbook's step 3a, and it is the only thing that
                      preserves the shadow run's sessions before
                      `import --replace` deletes them.

exit 0 pass · 1 a Tier-1 mismatch or a content difference · 2 a Tier-2 review
stop · 3 usage.";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Positional args and a few flags, no clap: two subcommands with fixed
    // arity do not need an argument parser, and this mirrors
    // `recall_bench.rs:811`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match argv.as_slice() {
        ["snapshot", rest @ ..] => match SnapshotArgs::parse(rest) {
            Some(parsed) => snapshot(&parsed.out, &parsed.drop_banks).await,
            None => return usage(),
        },
        ["import", rest @ ..] => match ImportArgs::parse(rest) {
            Some(parsed) => run_import(parsed).await,
            None => return usage(),
        },
        ["verify", rest @ ..] => match VerifyArgs::parse(rest) {
            // `verify` is the only subcommand whose *success* can carry a
            // non-zero exit, so it returns the code rather than `()`.
            Some(parsed) => match run_verify(parsed) {
                Ok(code) => return std::process::ExitCode::from(code),
                Err(e) => Err(e),
            },
            None => return usage(),
        },
        _ => return usage(),
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

/// **3, not 2.** `verify` uses 2 for a Tier-2 review stop, so usage cannot
/// share it — a script that treats "you called it wrong" as "a human should
/// look at the adjacency numbers" is worse than one that treats it as neither.
fn usage() -> std::process::ExitCode {
    eprintln!("{USAGE}");
    std::process::ExitCode::from(3)
}

/// `--drop-bank` is repeatable and defaults to nothing.
///
/// Naming a bank is a claim that it holds nothing, re-checked on every run and
/// again at `verify`. An operator with no such claim to make passes none and
/// loses nothing: an unnamed empty bank is snapshotted anyway and skipped at
/// import for having an empty archive.
struct SnapshotArgs {
    out: PathBuf,
    drop_banks: Vec<String>,
}

impl SnapshotArgs {
    fn parse(argv: &[&str]) -> Option<SnapshotArgs> {
        let mut out = None;
        let mut drop_banks = Vec::new();
        let mut rest = argv.iter();
        while let Some(arg) = rest.next() {
            match *arg {
                "--out" => out = Some(PathBuf::from(rest.next()?)),
                "--drop-bank" => drop_banks.push((*rest.next()?).to_string()),
                _ => return None,
            }
        }
        Some(SnapshotArgs {
            out: out?,
            drop_banks,
        })
    }
}

async fn snapshot(out: &Path, drop_banks: &[String]) -> migrate::Result<()> {
    println!("snapshot -> {}", out.display());
    let dropped = drop_banks.iter().map(String::as_str).collect();
    let lines = migrate::snapshot::run(out, &dropped).await?;
    for line in &lines {
        println!("{line}");
    }
    // The operator's own independent check is `sha256sum -c SHA256SUMS`; this
    // is ours, run immediately so a snapshot that cannot verify itself never
    // reaches the runbook's next line.
    migrate::snapshot::verify_sha256sums(out)?;
    println!("SHA256SUMS written and verified");
    Ok(())
}

struct ImportArgs {
    snapshot: PathBuf,
    db: PathBuf,
    replace: bool,
    defer_embeddings: bool,
}

impl ImportArgs {
    fn parse(argv: &[&str]) -> Option<ImportArgs> {
        let (mut snapshot, mut db) = (None, None);
        let (mut replace, mut defer) = (false, false);
        let mut rest = argv.iter();
        while let Some(arg) = rest.next() {
            match *arg {
                "--snapshot" => snapshot = Some(PathBuf::from(rest.next()?)),
                "--db" => db = Some(PathBuf::from(rest.next()?)),
                "--replace" => replace = true,
                "--defer-embeddings" => defer = true,
                _ => return None,
            }
        }
        Some(ImportArgs {
            snapshot: snapshot?,
            db: db?,
            replace,
            defer_embeddings: defer,
        })
    }
}

async fn run_import(args: ImportArgs) -> migrate::Result<()> {
    println!(
        "import {} -> {}{}",
        args.snapshot.display(),
        args.db.display(),
        if args.replace { " (--replace)" } else { "" }
    );

    let cfg = Config::load().map_err(|e| migrate::MigrateError::Store {
        message: format!("reading the daemon configuration: {e}"),
    })?;
    // The embedder is loaded whatever `--defer-embeddings` says: observations
    // cannot be deferred, because `consolidate::insert_observation` takes the
    // vector by value (`consolidate.rs:115-121`).
    let started = std::time::Instant::now();
    let embedder = load_embedder(&cfg).await?;
    println!("embedder loaded in {:.1}s", started.elapsed().as_secs_f64());

    let started = std::time::Instant::now();
    let reports = import::run(&import::Options {
        snapshot: &args.snapshot,
        db: &args.db,
        replace: args.replace,
        cfg: &cfg,
        embed: &|texts| embedder.embed_batch(texts),
        drain: (!args.defer_embeddings).then(|| embedder.clone()),
    })
    .await?;

    for report in &reports {
        println!("{}", report.line());
    }
    println!("wall {:.1}s", started.elapsed().as_secs_f64());

    // A count that does not reconcile is a failed import, not a printed note.
    // The per-bank line is this PR's AC-3 evidence and gets pasted into a
    // review; a run that exits 0 while a line reads `MISMATCH` teaches the
    // reader that the line is decoration. MG-2 is still the gate — this is
    // only the part the importer can already see.
    let broken: Vec<&str> = reports
        .iter()
        .filter(|r| !r.reconciles())
        .map(|r| r.bank_id.as_str())
        .collect();
    if !broken.is_empty() {
        return Err(migrate::MigrateError::Store {
            message: format!(
                "{} bank(s) did not reconcile against the frozen /stats: {}",
                broken.len(),
                broken.join(", ")
            ),
        });
    }
    Ok(())
}

/// The 133 MB ONNX model, loaded once and used for both halves of the import:
/// the observation vectors `insert_observation` demands by value, and — unless
/// `--defer-embeddings` — the fact backlog `embed_task::drain_once` drains.
///
/// **No database is opened here.** `import::run` opens the target itself and
/// builds `drain_once`'s `AppState` around that handle; opening `cfg.db_path`
/// from this binary would migrate the *live daemon's* database as a side
/// effect of a rehearsal that was supposed to touch nothing.
async fn load_embedder(cfg: &Config) -> migrate::Result<Arc<memgardend::embed::Embedder>> {
    let embedding = cfg.embedding.clone();
    tokio::task::spawn_blocking(move || memgardend::embed::Embedder::load(&embedding))
        .await
        .map_err(|e| migrate::MigrateError::Store {
            message: e.to_string(),
        })?
        .map(Arc::new)
        .map_err(|e| migrate::MigrateError::Store {
            message: e.to_string(),
        })
}

struct VerifyArgs {
    snapshot: PathBuf,
    db: PathBuf,
    out: Option<PathBuf>,
    sample: usize,
    seed: u64,
    accept_tier2: Option<String>,
    dump_only: bool,
}

impl VerifyArgs {
    fn parse(argv: &[&str]) -> Option<VerifyArgs> {
        let (mut snapshot, mut db, mut out, mut accept) = (None, None, None, None);
        let (mut sample, mut seed, mut dump_only) = (50usize, 1u64, false);
        let mut rest = argv.iter();
        while let Some(arg) = rest.next() {
            match *arg {
                "--snapshot" => snapshot = Some(PathBuf::from(rest.next()?)),
                "--db" => db = Some(PathBuf::from(rest.next()?)),
                "--out" => out = Some(PathBuf::from(rest.next()?)),
                "--sample" => sample = rest.next()?.parse().ok()?,
                "--seed" => seed = rest.next()?.parse().ok()?,
                "--accept-tier2" => accept = Some((*rest.next()?).to_string()),
                "--dump-only" => dump_only = true,
                _ => return None,
            }
        }
        Some(VerifyArgs {
            snapshot: snapshot?,
            db: db?,
            out,
            sample,
            seed,
            accept_tier2: accept,
            dump_only,
        })
    }
}

fn run_verify(args: VerifyArgs) -> migrate::Result<u8> {
    let report = migrate::verify::run(&migrate::verify::Options {
        snapshot: &args.snapshot,
        db: &args.db,
        sample: args.sample,
        seed: args.seed,
        accept_tier2: args.accept_tier2.as_deref(),
        dump_only: args.dump_only,
    })?;
    print!("{}", report.table());
    // Printed always, not only on a review stop: the operator who needs it is
    // the one deciding whether to accept, and making them re-run to learn the
    // hash is how a re-entry criterion becomes unused.
    println!("acceptance hash: {}", report.acceptance_hash());
    if let Some(path) = &args.out {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&report).expect("Report serializes"),
        )
        .map_err(|e| migrate::MigrateError::Store {
            message: format!("writing {}: {e}", path.display()),
        })?;
        println!("report -> {}", path.display());
    }
    Ok(report.verdict.exit_code())
}
