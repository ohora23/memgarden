//! `mg-migrate import` — the frozen archive into this store, with **our**
//! vectors and **our** derived links.
//!
//! # What is copied and what is rebuilt
//!
//! Exactly one link type survives transfer: `caused_by`, because it is the
//! only *authored* one — the extraction LLM emitted it as a relation between
//! two facts it had just produced, and legacy carries it verbatim in the
//! archive. `temporal` and `semantic` are **rebuilt from the migrated facts by
//! our own rules**, and `entity` is written by neither system
//! (`engine/memories/pg/counts.py:47-49` derives legacy's 4,123 at `/stats`
//! time; `links.rs:6-8` is our matching decision).
//!
//! Rebuilding is not the cheap choice, it is the only correct one:
//!
//! * a **semantic** edge is a function of the embedding space, and the export
//!   carries no vectors at all (`export.py:171-193`). Importing legacy's edges
//!   into a database whose vectors are ours would leave the graph arm
//!   asserting adjacencies `search::knn` could never reproduce — the failure
//!   `0005_embedding_model.sql:1-12` exists to prevent;
//! * a **temporal** edge is a different function on each side. Legacy's
//!   neighbour query (`engine/db/ops_postgresql.py:562-593`) takes the 20
//!   nearest by `event_date` in each direction and applies **no 24-hour
//!   predicate anywhere**, while `links.rs:69` filters on one. Measured, three
//!   replay orders: our rule over this archive gives 70,192 / 68,781 / 69,771
//!   edges against legacy's 43,637 — **1.58-1.61x, and ordering moves it by
//!   2 %**. The counts cannot be made equal, so `verify` gates on our own rule
//!   instead and reports the ratio.
//!
//! # There is no transaction around a bank, and the marker is why that is safe
//!
//! Every store helper here opens its own `BEGIN IMMEDIATE` on its own pooled
//! connection (`lib.rs:74-82`), and the composable `_tx` variants are
//! `pub(crate)`. Nesting them deadlocks against the pool's own write lock and
//! fails after `busy_timeout 5000` (`conn.rs:44`), and making seven helpers
//! public to buy atomicity for a one-time binary is the wrong trade — the
//! store's write API is shaped for the daemon.
//!
//! So a failed bank leaves a **partial** bank, and the whole design is that
//! partial is never *silent*: [`MARKER_KEY`] goes into the bank row's
//! `disposition` before the first node and flips to `done` after the last
//! step. `import` refuses a bank whose marker says `running` without
//! `--replace`, and `verify` fails Tier 1 on it.
//!
//! # Blocking store calls are made directly, not through `spawn_blocking`
//!
//! The workspace rule (`.omc/plans/phase-d-impl.md` Cross-PR rule 5) exists so
//! a long store write cannot stall the *daemon's* runtime while it is serving
//! recall. This binary is `#[tokio::main]` with exactly one task on it and
//! refuses to run at all against the database a live daemon holds
//! ([`assert_daemon_not_holding`]), so there is nothing to stall.
//! `recall_bench.rs:219-284` is the same shape for the same reason.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use memgarden_core::config::Config;
use memgarden_core::types::FactType;
use memgarden_store::consolidate::RunCounts;
use memgarden_store::graph::NewLink;
use memgarden_store::models::NewNode;
use memgarden_store::nodes::NewNodeWithTags;
use memgarden_store::{Db, banks, consolidate, documents, graph, nodes};
use serde_json::{Map, Value, json};

use super::archive::{BankArchive, TransferDocument, TransferFact};
use super::snapshot::{self, Stats, sha256_hex};
use super::{MigrateError, Result, load_stats};
use crate::links::{self, CAUSAL_LINK_WEIGHT, TimedNode};
use crate::state::AppState;
use crate::{embed_task, entities};

/// The key `import` writes into the bank row's `disposition` JSON.
///
/// `banks.disposition` already has a `json_valid` CHECK (`0001_init.sql:13`)
/// and `banks::update` already supports a partial write (`banks.rs:60`), so
/// the marker costs two statements, no new table and no migration.
///
/// `ponytail:` it records *whether* a run finished, not how far it got. If a
/// partial bank ever needs resuming rather than redoing, that is when a step
/// counter earns its keep.
pub const MARKER_KEY: &str = "mg_import";

/// `drain_once` returns on the **first** embedder error with no retry
/// (`embed_task.rs:110-124`), so "call until the backlog is empty" is an
/// unbounded spin against an embedder that will not load. Three calls, and the
/// backlog must shrink between two of them.
const MAX_DRAIN_CALLS: usize = 3;

/// How `import` gets its observation vectors.
///
/// A parameter rather than a direct [`crate::embed::Embedder`] call, for two
/// reasons that point the same way. `consolidate::insert_observation` takes
/// the embedding **by value** (`consolidate.rs:115-121`), so observations
/// cannot use the backlog the facts use and the importer has to embed them
/// itself — and the production vector source is a 133 MB ONNX model, which
/// B1's precedent keeps out of unit tests. This is the seam that lets
/// [`run`] itself be tested rather than only the pure functions under it.
pub type EmbedBatch<'a> = &'a dyn Fn(&[String]) -> anyhow::Result<Vec<Vec<f32>>>;

pub struct Options<'a> {
    /// The unpacked snapshot directory `mg-migrate snapshot --out` wrote.
    pub snapshot: &'a Path,
    pub db: &'a Path,
    /// Purge each migrated bank before writing it. The only way into a
    /// non-empty bank, and the only way to reuse one whose marker says
    /// `running`.
    pub replace: bool,
    /// The daemon's own configuration — read for `bind` and `db_path`, which
    /// are the two inputs to [`assert_daemon_not_holding`]. Nothing else here
    /// reads it.
    pub cfg: &'a Config,
    pub embed: EmbedBatch<'a>,
    /// `None` leaves the **fact** embedding backlog for the restarted daemon
    /// (§Binding decisions #3's downtime option: the partial index
    /// `idx_memory_nodes_embed_backlog`, `0001_init.sql:52`, *is* the backlog,
    /// and `embed_task` drains it on the next tick). `Some` drains it here,
    /// which is also what writes the semantic links (`embed_task.rs:172`).
    ///
    /// Observations are never deferrable — `insert_observation` wants the
    /// vector in hand — so [`Options::embed`] is required either way, and in
    /// production both fields carry the same loaded model.
    ///
    /// The `AppState` `drain_once` wants is built **here**, around the
    /// database [`run`] opened, rather than handed in: a caller-built one
    /// would have to `Db::open` the same file a second time, and two pools
    /// over one database is a lock-contention design nothing needs.
    pub drain: Option<Arc<crate::embed::Embedder>>,
}

/// One bank's outcome, with legacy's own numbers beside ours so the PR body's
/// count table is the program's output rather than a hand transcription.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BankReport {
    pub bank_id: String,
    /// A bank that reached the snapshot with no content. Not migrated, for the
    /// same reason a `--drop-bank` bank is not: creating a bank row
    /// whose only content is a mission string puts a number in the AC-3 report
    /// that overstates what was verified, and `hook session-start`'s `POST
    /// /v1/banks` (`session_start.rs:159-166`) recreates it on first use.
    pub skipped_empty: bool,
    pub documents: usize,
    pub facts: usize,
    pub observations: usize,
    pub entities: usize,
    pub causal_links: usize,
    pub temporal_links: usize,
    pub node_sources: i64,
    pub watermark: i64,
    pub pending_embeddings: i64,
    pub legacy_documents: i64,
    pub legacy_nodes: i64,
    pub legacy_caused_by: i64,
}

impl BankReport {
    /// Whether every count that *can* equal legacy's does.
    ///
    /// This is a real comparison and not a formatting convention, and the
    /// distinction is the whole point. The first version of [`line`] printed
    /// `ok` unconditionally and a literal `==` between two numbers it never
    /// compared — which is the exact defect class this repo's review gate
    /// keeps catching: a check that reads like a check and cannot fail.
    /// Worse, the comment on the self-causal drop in `import_bank` cites the
    /// report as *"what makes a nonzero one visible"*, and it did not.
    ///
    /// [`line`]: BankReport::line
    pub fn reconciles(&self) -> bool {
        self.skipped_empty
            || (self.documents as i64 == self.legacy_documents
                && (self.facts + self.observations) as i64 == self.legacy_nodes
                && self.causal_links as i64 == self.legacy_caused_by)
    }

    /// The line the manual verification pastes: ours against legacy's, for
    /// every count that Tier 1 can gate on, plus the ones only we have.
    pub fn line(&self) -> String {
        if self.skipped_empty {
            return format!("skip {}: empty archive, not migrated", self.bank_id);
        }
        let cmp = |ours: i64, legacy: i64| if ours == legacy { "==" } else { "!=" };
        format!(
            "{} {}: docs {} {} {} | nodes {} {} {} | causal {} {} {} | obs {} | sources {} | \
             temporal {} | entities {} | pending {} | watermark {}",
            if self.reconciles() {
                "ok  "
            } else {
                "MISMATCH"
            },
            self.bank_id,
            self.documents,
            cmp(self.documents as i64, self.legacy_documents),
            self.legacy_documents,
            self.facts + self.observations,
            cmp((self.facts + self.observations) as i64, self.legacy_nodes),
            self.legacy_nodes,
            self.causal_links,
            cmp(self.causal_links as i64, self.legacy_caused_by),
            self.legacy_caused_by,
            self.observations,
            self.node_sources,
            self.temporal_links,
            self.entities,
            self.pending_embeddings,
            self.watermark,
        )
    }
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

/// Imports every bank archive under `opts.snapshot` into `opts.db`.
///
/// **Every guard runs over every bank before the first row is written.** A
/// multi-bank run that refused the fourth bank after importing three would
/// leave three banks that need `--replace` to retry, which is a worse outcome
/// than a refusal that costs nothing.
pub async fn run(opts: &Options<'_>) -> Result<Vec<BankReport>> {
    assert_daemon_not_holding(opts.cfg, opts.db)?;
    snapshot::verify_sha256sums(opts.snapshot)?;

    let archives = super::archive::load_dir(opts.snapshot)?;
    let oracle = load_stats(opts.snapshot)?;
    let missions = load_banks(opts.snapshot)?;
    // Every file `SHA256SUMS` covers is already pinned by it, so hashing that
    // one file identifies the whole snapshot. The marker records it, which is
    // how `verify` catches a bank imported from a *different* snapshot than
    // the one being verified rather than counting it.
    let snapshot_id = sha256_hex(
        &std::fs::read(opts.snapshot.join("SHA256SUMS"))
            .map_err(|e| MigrateError::io(opts.snapshot.join("SHA256SUMS"), e))?,
    );

    let mut work: Vec<(&BankArchive, &Stats)> = Vec::new();
    for archive in &archives {
        let stats = oracle
            .get(&archive.bank_id)
            .ok_or_else(|| MigrateError::StatsMissing {
                bank: archive.bank_id.clone(),
            })?;
        // D1's own assertions, re-run against the bytes on disk. The snapshot
        // asserted them at freeze time; a rehearsal archive can be older than
        // this binary, and re-reading them here costs milliseconds.
        snapshot::assert_integrity(archive, stats)?;
        assert_importable(archive, stats)?;
        work.push((archive, stats));
    }

    let db = Arc::new(Db::open(opts.db).map_err(store)?);
    assert_schema_version(&db)?;
    let drain = opts
        .drain
        .clone()
        .map(|embedder| backlog_state(&db, opts.cfg, embedder))
        .transpose()?;
    for (archive, _) in &work {
        if !is_empty(archive) {
            assert_bank_available(&db, &archive.bank_id, opts.replace)?;
        }
    }

    let mut reports = Vec::with_capacity(work.len());
    for (archive, stats) in work {
        if is_empty(archive) {
            reports.push(BankReport {
                bank_id: archive.bank_id.clone(),
                skipped_empty: true,
                ..Default::default()
            });
            continue;
        }
        // `banks.json` is the only carrier of a mission and a disposition, so
        // an archived bank missing from it is the mirror of `StatsMissing` —
        // and without this it is silent: `write_marker` would create the bank
        // with a NULL mission and nothing would say the string was lost.
        let mission =
            missions
                .get(&archive.bank_id)
                .ok_or_else(|| MigrateError::BankNotListed {
                    bank: archive.bank_id.clone(),
                })?;
        let state = drain.as_ref().map(|(state, _)| state);
        reports.push(import_bank(&db, archive, stats, mission, &snapshot_id, state, opts).await?);
    }
    Ok(reports)
}

/// A bank whose archive carries nothing. Snapshotted (D1 archives any bank not
/// passed to `--drop-bank`, on purpose — deriving the drop set from "is it
/// empty right now" makes the emptiness assertion circular), but not migrated.
/// An operator who names no bank at all therefore still loses nothing here.
///
/// This is not hypothetical: `claude-code::memgarden` appeared in legacy
/// between D1's snapshot and D2's, with 0 nodes and 0 documents.
fn is_empty(archive: &BankArchive) -> bool {
    archive.documents.is_empty() && archive.observations.is_empty()
}

// ---------------------------------------------------------------------------
// guards
// ---------------------------------------------------------------------------

/// Refuses when a daemon is listening **and** the target is the database that
/// daemon holds open.
///
/// The plan's §Binding decisions #8 states the guard as "refuse if anything is
/// listening on the configured port", full stop — but that contradicts D2's
/// own manual verification, which imports the real snapshot into a scratch
/// database *with the daemon left running on :9100 untouched*, and it would
/// refuse every zero-downtime rehearsal the runbook asks for. The property
/// worth guarding is not "a daemon exists", it is "a second writer is about to
/// open the file a daemon already has". Both conditions, or no refusal.
///
/// `ponytail:` TOCTOU by construction — a daemon started one millisecond later
/// is not caught, and a daemon on a *different* port holding the same file is
/// not caught either. This is a footgun guard for the operator who forgot, not
/// a mutual-exclusion primitive; the real protection is SQLite's own write
/// lock plus the fact that the cutover is a two-line manual runbook. Upgrade
/// path if it is ever automated: an advisory `File::lock()` on a sidecar, the
/// same primitive Phase C already uses.
pub fn assert_daemon_not_holding(cfg: &Config, target: &Path) -> Result<()> {
    if !same_file(&cfg.db_path, target) {
        return Ok(());
    }
    let listening = std::net::TcpStream::connect_timeout(
        &parse_bind(&cfg.bind)?,
        std::time::Duration::from_millis(500),
    )
    .is_ok();
    if listening {
        return Err(MigrateError::DaemonListening {
            bind: cfg.bind.clone(),
            db: target.to_path_buf(),
        });
    }
    Ok(())
}

/// `canonicalize` where possible, so `~/…/memgarden.db` and a relative path to
/// the same file are one file rather than two. A path that does not exist yet
/// cannot be canonicalized and cannot be the database a running daemon holds
/// open either, so the lexical comparison is the right fallback.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn parse_bind(bind: &str) -> Result<std::net::SocketAddr> {
    bind.parse().map_err(|_| MigrateError::UnparseableBind {
        bind: bind.to_string(),
    })
}

/// `Db::open` migrates forward (`lib.rs:52-58`) but has nothing to say about a
/// database written by a **newer** binary: every migration entry sees
/// `version <= current` and skips (`migrate.rs:44-48`), leaving `user_version`
/// ahead of us and the import writing into a schema it does not know.
fn assert_schema_version(db: &Db) -> Result<()> {
    let found: i64 = db
        .read()
        .map_err(store)?
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| store(e.to_string()))?;
    if found != memgarden_store::LATEST_VERSION {
        return Err(MigrateError::SchemaVersionMismatch {
            found,
            supported: memgarden_store::LATEST_VERSION,
        });
    }
    Ok(())
}

/// The target bank must be empty and unmarked, or `--replace` must say so.
///
/// Both halves are needed and neither implies the other: a bank that failed at
/// step 2 has **zero** nodes and a `running` marker, and a bank the shadow run
/// wrote into has nodes and no marker at all.
fn assert_bank_available(db: &Db, bank_id: &str, replace: bool) -> Result<()> {
    if replace {
        return Ok(());
    }
    if let Some(bank) = banks::get(db, bank_id).map_err(store)?
        && marker_state(bank.disposition.as_deref()) == Some("running".to_string())
    {
        return Err(MigrateError::ImportInProgress {
            bank: bank_id.to_string(),
        });
    }
    let nodes = nodes::count(db, bank_id).map_err(store)?;
    if nodes != 0 {
        return Err(MigrateError::BankNotEmpty {
            bank: bank_id.to_string(),
            nodes,
        });
    }
    Ok(())
}

fn marker_state(disposition: Option<&str>) -> Option<String> {
    serde_json::from_str::<Value>(disposition?)
        .ok()?
        .get(MARKER_KEY)?
        .get("state")?
        .as_str()
        .map(str::to_string)
}

/// The refusals D1 deliberately left to D2, all of them "before any write" and
/// all of them about something *disappearing* rather than disagreeing.
fn assert_importable(archive: &BankArchive, stats: &Stats) -> Result<()> {
    let bank = archive.bank_id.as_str();
    let manifest = &archive.manifest;

    // A `"bank"` archive carries mental models, directives and webhooks in
    // files `load_dir` does not read (`schema.py:149`), so importing one would
    // move the documents and leave the rest behind without a word. Measured
    // `"documents"` in all five banks.
    if manifest.archive_type != super::archive::ArchiveType::Documents {
        return Err(MigrateError::UnsupportedArchiveType {
            bank: bank.to_string(),
            archive_type: format!("{:?}", manifest.archive_type),
        });
    }
    // The plan puts mental models out of scope with *"there is nothing to
    // migrate"*, measured `mental_model_count = 0` in every manifest. That is
    // a claim worth asserting rather than assuming, and `--replace` makes the
    // cost of being wrong concrete: it deletes the target bank's
    // `mental_models` (§Binding decisions #5d), so a legacy bank that grew one
    // would have it dropped on this side *and* not carried from that one.
    let counters = [
        ("mental_model_count", manifest.mental_model_count),
        ("directive_count", manifest.directive_count),
        ("webhook_count", manifest.webhook_count),
    ];
    for (field, count) in counters {
        if count != 0 {
            return Err(MigrateError::UnsupportedArchiveContent {
                bank: bank.to_string(),
                field,
                count,
            });
        }
    }
    // `includes_history` was the last of D1's deferred list: parsed, and
    // asserted against nothing. `false` in all five banks, and nothing in
    // `schema.py` says what a `true` adds — which is exactly why accepting one
    // is a guess about content we would then not carry.
    if manifest.includes_history {
        return Err(MigrateError::UnsupportedArchiveContent {
            bank: bank.to_string(),
            field: "includes_history",
            count: 1,
        });
    }

    for document in &archive.documents {
        let facts = document.facts.len() as i64;
        for (index, fact) in document.facts.iter().enumerate() {
            // D1 refuses a non-null `observation_scopes` on an *observation*,
            // matching the plan's list, and its deferred list records that the
            // same field on a *fact* is unchecked. It is the same silent drop
            // — there is no MemGarden column for it, and the field being
            // known-but-unused is exactly what `deny_unknown_fields`
            // structurally cannot catch. Measured null in all 3,541 facts.
            if let Some(scopes) = &fact.observation_scopes {
                return Err(MigrateError::FactScopesUnsupported {
                    bank: bank.to_string(),
                    document: document.id.clone(),
                    fact_index: index,
                    scopes: serde_json::to_string(scopes).unwrap_or_else(|_| "?".to_string()),
                });
            }
            for relation in &fact.causal_relations {
                // An ordinal into this document's own `facts` array
                // (`schema.py:35-43`), typed `int` by legacy — so a negative
                // or past-the-end value parses and has to be rejected by name.
                // Checked before the write because `graph::insert_links` would
                // otherwise take a foreign key to a node that does not exist
                // and fail with a constraint error naming neither document nor
                // fact.
                if relation.target_fact_index < 0 || relation.target_fact_index >= facts {
                    return Err(MigrateError::CausalTargetOutOfRange {
                        bank: bank.to_string(),
                        document: document.id.clone(),
                        fact_index: index,
                        target: relation.target_fact_index,
                        facts,
                    });
                }
            }
        }
    }

    // Every `fact_type` and every timestamp, parsed here rather than where it
    // is used. Both are pure functions of the archive, and both used to fire
    // in step 3 or step 6 — after the bank row, the marker and the documents
    // had been written, leaving a bank that needs `--replace` to retry for an
    // error a read could have caught. `assert_importable`'s promise is that
    // its refusals are *before any write*, and a refusal that is nearly before
    // any write is not that.
    for document in &archive.documents {
        for (index, fact) in document.facts.iter().enumerate() {
            let draft = Draft::from_archive(bank, fact)?;
            assert_event_date_derivable(bank, &document.id, index, draft.event_date)?;
        }
    }
    for (index, observation) in archive.observations.iter().enumerate() {
        ms(bank, "occurred_end", &observation.occurred_end)?;
        let event_date = observation_event_date(bank, observation)?;
        assert_event_date_derivable(bank, "observations.json", index, event_date)?;
    }

    // `_load_observations` *skips* an observation whose sources fall outside
    // the exported set and logs it (`export.py:415-466`), so a source that
    // does not resolve means the archive disagrees with itself rather than
    // that legacy lost something. `insert_observation` filters unresolvable
    // ids in SQL and drops them **silently** (`consolidate.rs:111-114`), which
    // is right for the daemon and wrong here: it would land an observation
    // with fewer proofs than legacy recorded and nothing would say so.
    let known: std::collections::HashSet<(&str, i64)> = archive
        .documents
        .iter()
        .flat_map(|d| (0..d.facts.len() as i64).map(move |i| (d.id.as_str(), i)))
        .collect();
    for (index, observation) in archive.observations.iter().enumerate() {
        for source in &observation.sources {
            if !known.contains(&(source.document_id.as_str(), source.fact_index)) {
                return Err(MigrateError::ObservationSourceUnresolved {
                    bank: bank.to_string(),
                    index,
                    document: source.document_id.clone(),
                    fact_index: source.fact_index,
                });
            }
        }
    }

    // `document_metadata` is the one `/documents` field with no counterpart
    // anywhere in `schema.py`'s `TransferDocument` — which makes it precisely
    // the shape `deny_unknown_fields` structurally cannot see: not an unknown
    // field *in* the archive, but a field the archive does not have. Step 2
    // carries `retain_params.metadata` on the assumption that the two are the
    // same object, measured 25/25 at snapshot time. Asserted rather than
    // assumed, because the failure is silent: a document would land carrying
    // metadata legacy does not associate with it.
    let legacy: BTreeMap<&str, Option<&Value>> = stats
        .documents
        .iter()
        .map(|d| (d.id.as_str(), d.extra.get("document_metadata")))
        .collect();
    for document in &archive.documents {
        let ours = retain_metadata(document);
        let theirs = legacy.get(document.id.as_str()).copied().flatten();
        if normalize_metadata(ours) != normalize_metadata(theirs) {
            return Err(MigrateError::DocumentMetadataMismatch {
                bank: bank.to_string(),
                document: document.id.clone(),
                archive: ours.map(|v| v.to_string()).unwrap_or_default(),
                legacy: theirs.map(|v| v.to_string()).unwrap_or_default(),
            });
        }
    }

    Ok(())
}

/// Legacy's `event_date` is NOT NULL on its side — it exists precisely as the
/// fallback for a unit with neither `occurred_start` nor `mentioned_at`
/// (`schema.py:57-58`). Ours is derived and *can* be NULL, and a NULL
/// `event_date` is skipped by `temporal_links` (`links.rs:62-68`), so such a
/// node would land with legacy's date discarded, no temporal edge, and nothing
/// saying so.
///
/// Measured 0 of 3,541 facts and 0 of 1,747 observations today — which is the
/// same posture `original_text` has (25/25 non-null) and `original_text` is
/// asserted. Two fields with the same risk profile should not get opposite
/// treatments, and the one that was measured-not-asserted is the one that
/// rots.
fn assert_event_date_derivable(
    bank: &str,
    document: &str,
    index: usize,
    event_date: Option<i64>,
) -> Result<()> {
    if event_date.is_none() {
        return Err(MigrateError::EventDateNotDerivable {
            bank: bank.to_string(),
            document: document.to_string(),
            index,
        });
    }
    Ok(())
}

fn retain_metadata(document: &TransferDocument) -> Option<&Value> {
    document.retain_params.as_ref()?.get("metadata")
}

/// An absent key and an explicit `null` are the same absence here; anything
/// else compares as itself. Without this, a document legacy answers with
/// `"document_metadata": null` and an archive with no `retain_params` at all
/// would read as a mismatch.
fn normalize_metadata(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}

// ---------------------------------------------------------------------------
// one bank
// ---------------------------------------------------------------------------

async fn import_bank(
    db: &Arc<Db>,
    archive: &BankArchive,
    stats: &Stats,
    legacy_bank: &LegacyBank,
    snapshot_id: &str,
    drain: Option<&AppState>,
    opts: &Options<'_>,
) -> Result<BankReport> {
    let bank_id = archive.bank_id.as_str();
    let mut report = BankReport {
        bank_id: archive.bank_id.clone(),
        documents: archive.documents.len(),
        facts: archive.fact_count(),
        observations: archive.observations.len(),
        legacy_documents: stats.stats.total_documents,
        legacy_nodes: stats.stats.total_nodes,
        legacy_caused_by: stats.stats.caused_by(),
        ..Default::default()
    };

    if opts.replace {
        purge(db, bank_id)?;
    }

    // --- 0/1. the bank row, its legacy mission, and the marker -------------
    let disposition = legacy_bank.disposition.clone();
    let mission = legacy_bank.mission.as_deref();
    write_marker(
        db,
        bank_id,
        mission,
        &disposition,
        marker(snapshot_id, false),
    )?;

    // --- 2. documents ------------------------------------------------------
    let mut document_ids: BTreeMap<&str, i64> = BTreeMap::new();
    for document in &archive.documents {
        // Asserted non-null by `snapshot::assert_integrity` before we get
        // here (`schema.py:125` types it `str | None`).
        let text = document.original_text.as_deref().unwrap_or_default();
        let content_hash = sha256_hex(text.as_bytes());
        let metadata = document_metadata(document);
        let upsert = documents::upsert(
            db,
            bank_id,
            &document.id,
            // Legacy's own document has no title; retain uses the session id
            // (`routes/retain.rs:342-349`) and the archive's `id` *is* that
            // session id, which `doc_key` already carries.
            None,
            &metadata,
            &content_hash,
        )
        .map_err(store)?;
        // The `set_content_hash` idiom, the one retain follows
        // (`retain/mod.rs:391`): `upsert`'s docstring describes a `metadata`
        // that already carries `content_sha256`, but the hash means "this
        // exact content is fully ingested" and is stamped on completion
        // (`documents.rs:86-92`). An import that stamped it up front would
        // make a half-written document a permanent duplicate.
        documents::set_content_hash(db, upsert.id, &content_hash).map_err(store)?;
        document_ids.insert(document.id.as_str(), upsert.id);
    }

    // --- 3. facts ----------------------------------------------------------
    let mut drafts: Vec<Draft> = Vec::with_capacity(report.facts);
    let mut metadata: Vec<String> = Vec::with_capacity(report.facts);
    for document in &archive.documents {
        for (index, fact) in document.facts.iter().enumerate() {
            drafts.push(Draft::from_archive(bank_id, fact)?);
            metadata.push(node_metadata(fact, &document.id, index));
        }
    }
    let flat: Vec<(&TransferDocument, &TransferFact)> = archive
        .documents
        .iter()
        .flat_map(|d| d.facts.iter().map(move |f| (d, f)))
        .collect();
    let items: Vec<NewNodeWithTags> = flat
        .iter()
        .zip(&drafts)
        .zip(&metadata)
        .map(|(((document, fact), draft), metadata)| NewNodeWithTags {
            node: NewNode {
                bank_id,
                document_id: document_ids.get(document.id.as_str()).copied(),
                fact_type: draft.fact_type,
                text: &fact.text,
                // `""` is legal per `schema.py` and absent from all 3,541 live
                // facts; `NewNode.context` wants the absence, not the empty
                // string (`recall_bench.rs:209`).
                context: fact.context.as_deref().filter(|c| !c.is_empty()),
                event_date: draft.event_date,
                occurred_start: draft.occurred_start,
                occurred_end: draft.occurred_end,
                mentioned_at: draft.mentioned_at,
                metadata: Some(metadata.as_str()),
            },
            tags: &fact.tags,
        })
        .collect();
    let fact_ids = nodes::insert_batch(db, &items).map_err(store)?;
    drop(items);

    // `(document uuid, fact ordinal)` -> our rowid. The archive's own
    // observation provenance uses that key (`export.py:459-462`) and it is the
    // only candidate measured unique across all 3,541 facts — `text` alone
    // collides 101 times.
    let mut by_key: BTreeMap<(&str, i64), i64> = BTreeMap::new();
    let mut offset = 0usize;
    for document in &archive.documents {
        for index in 0..document.facts.len() {
            by_key.insert(
                (document.id.as_str(), index as i64),
                fact_ids[offset + index],
            );
        }
        offset += document.facts.len();
    }

    // --- 4. entities -------------------------------------------------------
    write_entities(db, bank_id, archive, &drafts, &fact_ids)?;
    // Counted from the table, not summed from the per-document batches:
    // `write_entities` returns the map for *its* batch and upserts on
    // `(bank_id, canonical_name)`, so a name appearing in ten documents would
    // contribute ten to a running total while creating one row. Every fixture
    // is single-document, so `sum == distinct` held in every test — the number
    // only diverges on the 22-document bank, which is the one the AC-3 table
    // reports.
    report.entities = read_i64(
        db,
        "SELECT count(*) FROM entities WHERE bank_id = ?1",
        bank_id,
    )? as usize;

    // --- 5. causal links (the only type copied rather than derived) --------
    let mut causal: Vec<NewLink> = Vec::new();
    for document in &archive.documents {
        for (index, fact) in document.facts.iter().enumerate() {
            let from = by_key[&(document.id.as_str(), index as i64)];
            for relation in &fact.causal_relations {
                let to = by_key[&(document.id.as_str(), relation.target_fact_index)];
                // `links::causal_links` cannot be called: it reads
                // `extract::parse::ParsedFact`, the shape our own LLM pass
                // produces, and the archive is not that shape. Its two rules
                // are ported instead — flat `CAUSAL_LINK_WEIGHT`
                // (`causal_links.py:18`) and self-links dropped, since "this
                // fact was caused by itself" is extraction noise and the row
                // would be a self-loop. Measured 0 self-causal relations in
                // all 200; the report's count is what makes a nonzero one
                // visible against `/stats.caused_by`.
                if to != from {
                    causal.push(NewLink {
                        from_node_id: from,
                        to_node_id: to,
                        link_type: "caused_by",
                        weight: CAUSAL_LINK_WEIGHT,
                    });
                }
            }
        }
    }
    report.causal_links = graph::insert_links(db, &causal, now()).map_err(store)?;

    // --- 6. observations, BEFORE the temporal pass -------------------------
    // `temporal_links` pairs same-`fact_type` only (`links.rs:66-67`), so
    // observations link only to other observations — and only to the ones
    // already inserted when the pass runs. The plan's first draft had these
    // two steps the other way round and gave 1,747 nodes no temporal edges at
    // all.
    let observation_ids = write_observations(db, bank_id, archive, &by_key, opts.embed)?;
    report.node_sources = count_node_sources(db, bank_id)?;

    // --- 7. temporal links, once over every node in the bank ---------------
    let mut timed: Vec<TimedNode> = fact_ids
        .iter()
        .zip(&drafts)
        .filter_map(|(id, draft)| {
            draft.event_date.map(|event_date| TimedNode {
                id: *id,
                fact_type: draft.fact_type.as_str().to_string(),
                event_date,
            })
        })
        .collect();
    for (id, observation) in observation_ids.iter().zip(&archive.observations) {
        if let Some(event_date) = observation_event_date(bank_id, observation)? {
            timed.push(TimedNode {
                id: *id,
                fact_type: FactType::Observation.as_str().to_string(),
                event_date,
            });
        }
    }
    let temporal = links::temporal_links(&timed, &timed);
    report.temporal_links = graph::insert_links(db, &temporal, now()).map_err(store)?;

    // --- 8. fact embeddings, and the semantic links that ride with them ----
    if let Some(state) = drain {
        drain_backlog(db, state, bank_id).await?;
    }
    report.pending_embeddings = pending_embeddings(db, bank_id)?;

    // --- 9. the consolidation watermark ------------------------------------
    // Eligibility is `id > COALESCE(MAX(watermark), 0)`
    // (`consolidate.rs:314-330`, `:574-582`), and skipping this breaks it in
    // both directions. Without a row the daemon hands the **whole** migrated
    // corpus to the background task within one poll interval of restart and
    // re-derives against Ollama every observation we just imported — the AC-3
    // node count drifting minutes after `verify` printed PASS. And after
    // `--replace`, `memory_nodes.id` restarts at 1 (`INTEGER PRIMARY KEY`, no
    // `AUTOINCREMENT`, `0001_init.sql:30`) while a stale watermark stays put:
    // measured live on :9100, `watermark = 12` against `max(id) = 24`, which
    // would leave the first 12 imported nodes permanently invisible to
    // consolidation. That is why `purge` deletes `consolidation_runs` too.
    let run_id = consolidate::start_run(db, bank_id).map_err(store)?;
    report.watermark = max_node_id(db, bank_id)?;
    consolidate::finish_run(
        db,
        run_id,
        "done",
        RunCounts {
            facts_seen: report.facts as i64,
            ..Default::default()
        },
        Some(report.watermark),
        None,
    )
    .map_err(store)?;

    // --- 10. the marker, done ----------------------------------------------
    write_marker(
        db,
        bank_id,
        mission,
        &disposition,
        marker(snapshot_id, true),
    )?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// steps
// ---------------------------------------------------------------------------

/// `--replace`: everything in the bank that the migration owns, in one
/// transaction, before anything is written back.
///
/// Raw SQL through `Db::write` rather than a store helper, because there is no
/// helper for any of it and Cross-PR rule 5 forbids growing one for the
/// importer's sake — `nodes::delete` is per-row, and `banks::delete` would
/// cascade `retain_jobs` (`0002_retain_jobs.sql:9`), which must survive.
///
/// What is deleted, and why each one:
///
/// * `memory_nodes` cascades `links`, `node_tags`, `node_entities` and
///   `node_sources`, and drops `vec_nodes` through the trigger at
///   `0001_init.sql:91-93`;
/// * `documents` and `entities` (the latter cascading `entity_cooccurrences`);
/// * `mental_models`, which hangs off `banks` rather than off `memory_nodes`
///   (`0006_mental_models.sql:22`) and would otherwise survive as a stale
///   model over a replaced corpus — a wrong answer rather than a missing one,
///   with `vec_mental_models` still holding vectors for text nobody can reach.
///   Measured 0 rows on :9100 and `mental_model_count = 0` in all four
///   manifests, so nothing is lost either way;
/// * `consolidation_runs`, or the stale watermark survives the purge and hides
///   the front of the re-import;
/// * `sessions`, because `parity-gaps.md`'s standing decision is that every
///   session restarts at offset 0 after cutover. **This is measurement data**
///   (AC-2/AC-6), and the runbook's step 3a dumps it before this runs.
///
/// `retain_jobs` rows are **spared**: a job left `Pending` whose row vanishes
/// resolves to a 404, and `cmd/retain.rs:498-504` reads a 404 as `Failed`,
/// which rolls the client cursor back and re-sends. Deleting them causes
/// re-ingestion, not cleanliness. Sparing them is not free either —
/// `retain_jobs.document_id` is `ON DELETE SET NULL`
/// (`0002_retain_jobs.sql:10`), so the `documents` delete above severs the
/// join that makes those rows AC-2 evidence.
fn purge(db: &Db, bank_id: &str) -> Result<()> {
    // The docstring says the runbook's step 3a dumps `sessions` first, and
    // nothing in the binary enforces that. Saying the number out loud is what
    // closes the gap between the two: an operator who skipped the dump finds
    // out while the terminal still says how much there was.
    let sessions = read_i64(
        db,
        "SELECT count(*) FROM sessions WHERE bank_id = ?1",
        bank_id,
    )?;
    if sessions > 0 {
        println!(
            "--replace: deleting {sessions} sessions row(s) for {bank_id} — AC-2/AC-6 \
             measurement data. The runbook dumps them before this runs."
        );
    }
    db.write(|tx| {
        for table in [
            "memory_nodes",
            "documents",
            "entities",
            "mental_models",
            "consolidation_runs",
            "sessions",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE bank_id = ?1"),
                rusqlite::params![bank_id],
            )
            .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
        }
        Ok(())
    })
    .map_err(store)
}

fn marker(snapshot_id: &str, done: bool) -> Value {
    json!({
        "state": if done { "done" } else { "running" },
        "at": now(),
        "snapshot": snapshot_id,
    })
}

/// Step 0/1 and step 10 are the same two statements, so they are one function.
///
/// `banks::create` is a plain `INSERT` with no upsert (`banks.rs:16-21`) and
/// fails against a bank that already exists — which on :9100 today is the one
/// the shadow run has been writing into. `banks::update` is the partial write
/// (`banks.rs:60-80`) and is what the marker flip uses.
fn write_marker(
    db: &Db,
    bank_id: &str,
    mission: Option<&str>,
    disposition: &Option<Value>,
    marker: Value,
) -> Result<()> {
    let mut object: Map<String, Value> = match disposition {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    object.insert(MARKER_KEY.to_string(), marker);
    let disposition = Value::Object(object).to_string();
    if banks::get(db, bank_id).map_err(store)?.is_some() {
        // `mission.map(Some)`, not `Some(mission)`. `banks::update` reads
        // `None` as "leave the column alone" and `Some(None)` as "set it to
        // NULL" (`banks.rs:56-59`), so passing `Some(mission)` through would
        // *erase* the mission of an existing bank whenever legacy's own is
        // null — and the bank that already exists on :9100 is one `hook
        // session-start` created with a mission of its own.
        banks::update(db, bank_id, mission.map(Some), Some(Some(&disposition))).map_err(store)
    } else {
        banks::create(db, bank_id, mission, Some(&disposition))
            .map(|_| ())
            .map_err(store)
    }
}

/// `retain_params.metadata` verbatim, plus the one field the store cannot
/// keep.
///
/// **`documents.created_at` is the import time, not legacy's.**
/// `documents::upsert` writes `now_ms()` (`documents.rs:73-77`) and a
/// migration does not get to reshape a store helper retain depends on, so
/// legacy's value is preserved here instead of being silently reset. Recorded
/// as a `parity-gaps.md` row; the re-entry criterion is a caller that orders
/// documents by creation.
fn document_metadata(document: &TransferDocument) -> String {
    let mut object: Map<String, Value> = match retain_metadata(document) {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    if let Some(created_at) = &document.created_at {
        object.insert("legacy_created_at".to_string(), json!(created_at));
    }
    Value::Object(object).to_string()
}

/// The archive's own `metadata` with the migration's join key merged in.
///
/// `legacy` is inserted **last** so it wins: a fact whose legacy metadata
/// happened to carry a `legacy` key would otherwise overwrite the only thing
/// MG-2's 50-sample diff can join on. There is no `legacy_ref` column and no
/// `migration_map` table — a JSON object in a column that already exists is
/// enough for a one-time 5,288-row join, and a column would be dead weight
/// from the day Phase F ends.
///
/// `ponytail:` the ceiling is a full-corpus join in RAM. If a future migration
/// is 10^6 rows, add
/// `CREATE INDEX … ON memory_nodes(json_extract(metadata,'$.legacy.document_id'))`.
/// Two more fields ride in the `legacy` object, and both are there because the
/// alternative is a silent drop:
///
/// * **`created_at`** — `nodes::insert_batch` stamps `now_ms()`
///   (`nodes.rs:59`), the same reason `documents.created_at` is the import
///   time and legacy's lives in `metadata.legacy_created_at`. Doing it for
///   documents and not for facts would have been an inconsistency nobody
///   could see from the schema.
/// * **`consolidation_failed_at`**, when set. §Binding decisions #5b collapses
///   the whole per-fact consolidation lifecycle into one `consolidation_runs`
///   watermark row, which says "everything up to this id is consolidated" —
///   and **one fact in the live corpus carries a
///   `consolidation_failed_at`**, censused. The watermark cannot express that
///   (it is a single rowid; lowering it to reach one fact would re-consolidate
///   every fact after it), so the flag is carried where it stays recoverable
///   instead of being asserted away. `consolidated_at` is *not* carried: the
///   watermark says exactly what it says, for 3,540 of 3,541 facts.
fn node_metadata(fact: &TransferFact, document_id: &str, fact_index: usize) -> String {
    let mut object: Map<String, Value> = fact
        .metadata
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let mut legacy = serde_json::Map::new();
    legacy.insert("document_id".to_string(), json!(document_id));
    legacy.insert("fact_index".to_string(), json!(fact_index));
    if let Some(created_at) = &fact.created_at {
        legacy.insert("created_at".to_string(), json!(created_at));
    }
    if let Some(failed_at) = &fact.consolidation_failed_at {
        legacy.insert("consolidation_failed_at".to_string(), json!(failed_at));
    }
    object.insert("legacy".to_string(), Value::Object(legacy));
    Value::Object(object).to_string()
}

/// Step 4: **normalize, and stop there.**
///
/// Three paths were on the table and the middle one is the right one, which
/// took a measurement to establish.
///
/// `recall_bench.rs:241-262` hands `graph::write_entities` the legacy names
/// **raw**. That is harmless for a corpus nothing ever retains into again, and
/// wrong here: legacy's canonical names are not normalized in our sense —
/// measured on the archive, `Agent`, `BM25`, `Claude`, `CE-9a` — while
/// `entities::normalize` is trim + lowercase (`entities.rs:30`) and
/// `write_entities` upserts on `(bank_id, canonical_name)`. Raw names would
/// leave every future retain's `claude` beside the migrated `Claude` as a
/// second entity, splitting the graph arm's co-membership signal from the
/// first prompt after cutover.
///
/// `retain::write_graph` (`retain/mod.rs:599-618`) goes further and calls
/// `entities::resolve_fact`, which is `normalize` **plus** a fuzzy pass
/// against the bank's existing entities. The first version of this function
/// did the same, on the reasoning that matching retain's path must be right.
/// It is not, and the corpus says so: over the four banks the fuzzy pass
/// dissolved **77 of 3,917 distinct normalized names** into other entities,
/// and 33 of those 77 have no plausible variant to have merged into —
/// `ce-4` into `ce-1`, `phase e` into `phase a`, `prd` into `pr`, `ci.yml`
/// into `cli.mjs`, `shell` into `schedule`. 22 mentions vanished with them.
///
/// The mechanism is in the scoring: `resolution_score` is
/// `ratio*0.5 + overlap*0.3 + temporal*0.2` (`entities.rs:160-176`), so two
/// names sharing a fact on the same day already hold **0.5 of the 0.6 gate
/// before their names are compared at all** — the effective name-similarity
/// bar is about 0.2 whenever co-occurrence and recency are both satisfied,
/// which in a bulk import they always are: every fact's names co-occur densely
/// and every date is clustered.
///
/// **And the fuzzy pass buys the migration nothing**, which is the part the
/// first version got backwards. Its job is to merge spelling variants — and
/// legacy already merged those, in legacy's own space, before exporting.
/// Normalization is the whole of what this migration needs.
///
/// Two things this deliberately does **not** claim, both of which an earlier
/// version of this comment did and review refuted:
///
/// * that the fuzzy pass is only dangerous here because retain's candidate set
///   is small. It is not small: `load_resolution_context` is
///   `WHERE bank_id = ?1 ORDER BY last_seen DESC LIMIT 5000`
///   (`graph.rs:54-72`) — **bank-wide**. The per-chunk quantity is `nearby`
///   (`entities.rs:224-228`), not the candidates. So the dense regime that
///   produced the 77 merges is not unique to the import;
/// * that a later `CE-4` is *guaranteed* to land on the migrated `ce-4` row.
///   `retain::write_graph` calls `resolve_fact` **before**
///   `graph::write_entities` (`retain/mod.rs:600-618`), so the upsert key sees
///   the resolved name rather than the normalized one, and `resolve_fact`
///   takes the argmax with no exact-match short-circuit. An exact match holds
///   `1.0*0.5` plus a temporal term that is **0** once the migrated entity's
///   `last_seen` — its legacy date — is months old, so a fresher, co-occurring
///   candidate can outscore it.
///
/// The second is a standing CE-7 property that this PR does not change and is
/// not entitled to change; `book/src/roadmap.md` records it with the one-line
/// shape of a fix and the measurement that would justify it.
///
/// Each fact carries its **own** date into `first_seen`/`last_seen`
/// (`entity_processing.py:28`) — a bank-wide stamp would flatten the 0.2
/// temporal term that retain's resolutions depend on.
fn write_entities(
    db: &Db,
    bank_id: &str,
    archive: &BankArchive,
    drafts: &[Draft],
    ids: &[i64],
) -> Result<()> {
    let now = now();
    let mentions: Vec<graph::EntityMentions> = archive
        .documents
        .iter()
        .flat_map(|d| d.facts.iter())
        .zip(drafts)
        .zip(ids)
        .filter(|((fact, _), _)| !fact.entities.is_empty())
        .map(|((fact, draft), id)| {
            (
                *id,
                entities::normalized_mentions(&fact.entities),
                draft.event_date.unwrap_or(now),
            )
        })
        .filter(|(_, names, _)| !names.is_empty())
        .collect();
    // One call for the bank rather than one per document. `write_entities`
    // folds mention counts and co-occurrence pairs across whatever it is
    // given, and the pairs are per node either way, so the batching changes
    // nothing but the number of transactions.
    graph::write_entities(db, bank_id, &mentions, now).map_err(store)?;
    Ok(())
}

/// Step 6. Embed, insert through the production path, then write the six
/// things that path has no parameter for.
///
/// `insert_observation` writes `uuid, bank_id, fact_type, text, embedding,
/// embedding_model, mentioned_at, created_at, updated_at` and **nothing else**
/// (`consolidate.rs:139-155`) — `mentioned_at` is `now`, and `event_date`,
/// `occurred_start`, `occurred_end`, `metadata` and the tags are never written
/// at all. That is right for the daemon, where an observation is created *now*
/// out of facts it has in hand. For a migration it drops:
///
/// * **four date columns on a third of the corpus.** All 1,747 live
///   observations carry an `event_date` and a `mentioned_at` (241 carry
///   `occurred_*` too), censused — and `mentioned_at` is the column MG-2's
///   50-sample diff joins observations by, so the wall-clock substitute is not
///   merely absent data, it is a *fabricated* value that reads as real;
/// * **§Binding decisions #4's observation identity key**,
///   `{"legacy":{"observation_of":[…]}}` — the archive's `sources` array
///   verbatim. The provenance survives in `node_sources` either way, but the
///   *join* MG-2 was specified to use would not;
/// * the tags MG-2's Tier-1 multiset gate counts.
///
/// It also makes the observations reproducible from the database rather than
/// only from the archive, which is what D3's Tier-1 temporal self-consistency
/// check needs: that check re-runs `links.rs:62-92` over the **migrated
/// nodes'** own `(fact_type, event_date)`, and observations whose `event_date`
/// is NULL would produce zero edges there against 34,804 stored.
///
/// (What it does *not* change is what step 7 writes. The temporal pass builds
/// its `TimedNode`s from the archive, not from the database, so the edges land
/// either way — an earlier draft of this comment claimed otherwise and the
/// test named after it would have passed with this whole function deleted.)
///
/// The tags ride in the same transaction rather than paying 1,747 more
/// `BEGIN IMMEDIATE`s for one `INSERT OR IGNORE` each through
/// `nodes::add_tags`.
fn write_observations(
    db: &Db,
    bank_id: &str,
    archive: &BankArchive,
    by_key: &BTreeMap<(&str, i64), i64>,
    embed: EmbedBatch<'_>,
) -> Result<Vec<i64>> {
    if archive.observations.is_empty() {
        return Ok(Vec::new());
    }
    // Raw text, as `consolidate::round::embed_one` does — the augmentation in
    // `embed_task.rs:107` is the *fact* path's, and an observation embedded
    // differently from the ones the daemon writes afterwards would dedup
    // against them across a seam.
    let texts: Vec<String> = archive
        .observations
        .iter()
        .map(|o| o.text.clone())
        .collect();
    let vectors = embed(&texts).map_err(|e| MigrateError::Embed {
        bank: bank_id.to_string(),
        message: e.to_string(),
    })?;
    if vectors.len() != texts.len() {
        return Err(MigrateError::Embed {
            bank: bank_id.to_string(),
            message: format!(
                "embedder returned {} vectors for {} observations",
                vectors.len(),
                texts.len()
            ),
        });
    }

    // Every timestamp is parsed **before** the first `insert_observation`.
    // Parsing after it is a path that writes 1,747 rows and then refuses, and
    // the recovery from that is `--replace` — for an error that is a pure
    // function of the archive and costs nothing to find early.
    let drafts = observation_drafts(bank_id, archive)?;

    let mut ids = Vec::with_capacity(texts.len());
    for (observation, vector) in archive.observations.iter().zip(&vectors) {
        // Duplicates are legal and real — 86 across the live corpus, all in
        // `claude-code::bank-b` — and they collapse here, because
        // `link_sources_tx` is `INSERT OR IGNORE` against the
        // `(observation_id, source_id)` PK (`consolidate.rs:638-650`).
        // `proof_count` is then derived from what survived
        // (`recount_proof_tx`, `:658-666`), which is why it is a Tier-2
        // *report* against legacy's stored value and never an assertion.
        let sources: Vec<i64> = observation
            .sources
            .iter()
            .map(|s| by_key[&(s.document_id.as_str(), s.fact_index)])
            .collect();
        ids.push(
            consolidate::insert_observation(db, bank_id, &observation.text, vector, &sources)
                .map_err(store)?,
        );
    }

    db.write(|tx| {
        for ((id, draft), observation) in ids.iter().zip(&drafts).zip(&archive.observations) {
            let tags = &observation.tags;
            tx.execute(
                "UPDATE memory_nodes
                 SET event_date = ?2, occurred_start = ?3, occurred_end = ?4,
                     mentioned_at = ?5, metadata = ?6
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    draft.event_date,
                    draft.occurred_start,
                    draft.occurred_end,
                    // Stamped **unconditionally**, not `coalesce`d over
                    // `insert_observation`'s `now`. A coalesce cannot write a
                    // NULL, so an archive `mentioned_at: null` would silently
                    // become the migration's wall clock — a value that exists
                    // nowhere in legacy, on the column MG-2's 50-sample diff
                    // joins observations by — and the post-condition "no
                    // observation has a NULL mentioned_at" would have been
                    // true however badly this line was broken.
                    draft.mentioned_at,
                    observation_metadata(observation),
                ],
            )
            .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            for tag in tags {
                tx.execute(
                    "INSERT OR IGNORE INTO node_tags (node_id, tag) VALUES (?1, ?2)",
                    rusqlite::params![id, tag],
                )
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            }
        }
        Ok(())
    })
    .map_err(store)?;

    Ok(ids)
}

/// Every observation's dates, by the same rule the facts use.
fn observation_drafts(bank_id: &str, archive: &BankArchive) -> Result<Vec<Draft>> {
    archive
        .observations
        .iter()
        .map(|observation| {
            Ok(Draft {
                fact_type: FactType::Observation,
                event_date: observation_event_date(bank_id, observation)?,
                occurred_start: ms(bank_id, "occurred_start", &observation.occurred_start)?,
                occurred_end: ms(bank_id, "occurred_end", &observation.occurred_end)?,
                mentioned_at: ms(bank_id, "mentioned_at", &observation.mentioned_at)?,
            })
        })
        .collect()
}

/// §Binding decisions #4's observation identity: the archive's `sources` array
/// verbatim, under the same `legacy` key the facts use.
///
/// Duplicates are **kept here** where `node_sources` collapses them: this is
/// what legacy said, and the collapse is what we did with it. MG-2 needs both
/// sides to explain the 2,200-against-2,114 difference from the database
/// alone, once the archive is gone.
fn observation_metadata(observation: &super::archive::TransferObservation) -> String {
    json!({
        "legacy": {
            "observation_of": observation
                .sources
                .iter()
                .map(|s| json!({"document_id": s.document_id, "fact_index": s.fact_index}))
                .collect::<Vec<_>>(),
        }
    })
    .to_string()
}

/// The `AppState` `embed_task::drain_once` takes, around the database this run
/// opened.
///
/// `drain_once` reads exactly three things from it — the loaded embedder,
/// `cfg.embedding.batch_size`, and nothing else — so everything below is the
/// minimum that type-checks. Ollama is pointed at loopback port 1 on purpose,
/// as `recall_bench.rs:757` does: the import runs no LLM extraction, and a
/// misconfiguration should fail fast rather than quietly reach a real model.
///
/// The `Receiver` is returned rather than dropped. Dropping it closes
/// `retain_tx`, and a closed channel is a footgun for anything added here
/// later; nothing in this module retains.
fn backlog_state(
    db: &Arc<Db>,
    cfg: &Config,
    embedder: Arc<crate::embed::Embedder>,
) -> Result<(
    AppState,
    tokio::sync::mpsc::Receiver<crate::retain::RetainTask>,
)> {
    let mut cfg = cfg.clone();
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.max_retries = 0;
    let ollama = Arc::new(crate::ollama::OllamaClient::new(cfg.ollama.clone()).map_err(store)?);
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    Ok((
        AppState {
            db: db.clone(),
            cfg: Arc::new(cfg),
            started_at_ms: now(),
            embedder: Arc::new(std::sync::RwLock::new(Some(embedder))),
            reranker: Default::default(),
            ollama,
            consolidating: Default::default(),
            refreshing: Default::default(),
            retain_tx,
        },
        retain_rx,
    ))
}

/// Step 8, bounded.
///
/// `drain_once` returns on the first embedder error with no retry
/// (`embed_task.rs:110-124`) — including the "model still loading" return at
/// `:80-82`, which looks exactly like a drained backlog from out here. So the
/// loop is bounded at [`MAX_DRAIN_CALLS`] **and** requires the backlog to
/// shrink between two calls: an embedder that will not load otherwise spins
/// forever against a backlog nothing is emptying.
async fn drain_backlog(db: &Arc<Db>, state: &AppState, bank_id: &str) -> Result<()> {
    let mut before = pending_embeddings(db, bank_id)?;
    for calls in 1..=MAX_DRAIN_CALLS {
        if before == 0 {
            return Ok(());
        }
        embed_task::drain_once(db, state).await;
        let after = pending_embeddings(db, bank_id)?;
        if after == 0 {
            return Ok(());
        }
        if after >= before {
            return Err(MigrateError::EmbeddingBacklogStalled {
                bank: bank_id.to_string(),
                pending: after,
                calls,
            });
        }
        before = after;
    }
    Err(MigrateError::EmbeddingBacklogStalled {
        bank: bank_id.to_string(),
        pending: before,
        calls: MAX_DRAIN_CALLS,
    })
}

// ---------------------------------------------------------------------------
// drafts and timestamps
// ---------------------------------------------------------------------------

/// A node's type and its four timestamps, resolved to what `NewNode` wants.
///
/// A struct rather than a tuple because three of the four are `Option<i64>`
/// and a positional swap between them would be a silent temporal corruption,
/// not a compile error (`recall_bench.rs:128-138`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Draft {
    fact_type: FactType,
    event_date: Option<i64>,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
}

impl Draft {
    fn from_archive(bank_id: &str, fact: &TransferFact) -> Result<Draft> {
        let occurred_start = ms(bank_id, "occurred_start", &fact.occurred_start)?;
        let mentioned_at = ms(bank_id, "mentioned_at", &fact.mentioned_at)?;
        Ok(Draft {
            // Not `unwrap_or(World)` as the bench does: an unrecognised
            // `fact_type` is a legacy shape change, and defaulting it would
            // file every one of them as a world fact with nothing saying so.
            fact_type: fact
                .fact_type
                .parse()
                .map_err(|_| MigrateError::UnknownFactType {
                    bank: bank_id.to_string(),
                    fact_type: fact.fact_type.clone(),
                })?,
            // `writes.py:80` parity, the same derivation
            // `retain::NodeDraft::build` uses. The archive's own `event_date`
            // is legacy's NOT NULL fallback (`schema.py:57-58`) and is carried
            // rather than consumed — measured: **0 of 3,541 facts have neither
            // `occurred_start` nor `mentioned_at`**, so the fallback has
            // nothing to add and consuming it would put a value in
            // `event_date` that our own rule would not produce.
            event_date: occurred_start.or(mentioned_at),
            occurred_start,
            occurred_end: ms(bank_id, "occurred_end", &fact.occurred_end)?,
            mentioned_at,
        })
    }
}

/// An observation's `event_date`, by the same rule the facts use.
fn observation_event_date(
    bank_id: &str,
    observation: &super::archive::TransferObservation,
) -> Result<Option<i64>> {
    Ok(
        ms(bank_id, "occurred_start", &observation.occurred_start)?.or(ms(
            bank_id,
            "mentioned_at",
            &observation.mentioned_at,
        )?),
    )
}

/// RFC 3339 (what legacy emits) to epoch ms.
///
/// Refuses rather than returning `None` on a value it cannot parse, which is
/// where this differs from `recall_bench.rs:186-191`. A bench that drops an
/// unreadable timestamp loses a data point; a migration that drops one loses
/// the node's place in the temporal graph and says nothing.
fn ms(bank_id: &str, field: &'static str, value: &Option<String>) -> Result<Option<i64>> {
    let Some(text) = value.as_deref().filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    text.parse::<jiff::Timestamp>()
        .map(|ts| Some(ts.as_millisecond()))
        .map_err(|_| MigrateError::BadTimestamp {
            bank: bank_id.to_string(),
            field,
            value: text.to_string(),
        })
}

// ---------------------------------------------------------------------------
// banks.json, and the small reads
// ---------------------------------------------------------------------------

/// One entry of the frozen `GET /v1/default/banks`. Only the two fields the
/// import writes are named; the rest of the row stays in the file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LegacyBank {
    pub bank_id: String,
    #[serde(default)]
    pub mission: Option<String>,
    #[serde(default)]
    pub disposition: Option<Value>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyBanks {
    banks: Vec<LegacyBank>,
}

/// `banks.json` is the only carrier of a bank's mission and disposition — the
/// transfer archive has neither.
fn load_banks(dir: &Path) -> Result<BTreeMap<String, LegacyBank>> {
    let banks: LegacyBanks = super::archive::read_json(&dir.join("banks.json"))?;
    Ok(banks
        .banks
        .into_iter()
        .map(|b| (b.bank_id.clone(), b))
        .collect())
}

fn count_node_sources(db: &Db, bank_id: &str) -> Result<i64> {
    read_i64(
        db,
        "SELECT count(*) FROM node_sources s
         JOIN memory_nodes n ON n.id = s.observation_id
         WHERE n.bank_id = ?1",
        bank_id,
    )
}

fn pending_embeddings(db: &Db, bank_id: &str) -> Result<i64> {
    read_i64(
        db,
        "SELECT count(*) FROM memory_nodes WHERE bank_id = ?1 AND embedding IS NULL",
        bank_id,
    )
}

fn max_node_id(db: &Db, bank_id: &str) -> Result<i64> {
    read_i64(
        db,
        "SELECT COALESCE(MAX(id), 0) FROM memory_nodes WHERE bank_id = ?1",
        bank_id,
    )
}

fn read_i64(db: &Db, sql: &str, bank_id: &str) -> Result<i64> {
    db.read()
        .map_err(store)?
        .query_row(sql, rusqlite::params![bank_id], |r| r.get(0))
        .map_err(|e| store(e.to_string()))
}

fn now() -> i64 {
    memgarden_core::now_ms()
}

fn store(e: impl std::fmt::Display) -> MigrateError {
    MigrateError::Store {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::test_support::Snapshot;
    use memgarden_core::EMBEDDING_DIM;

    const JCODE: &str = "claude-code::bank-a";
    const CMS: &str = "claude-code::bank-b";

    /// A deterministic stand-in for the 133 MB ONNX model.
    ///
    /// One basis vector per text, so every vector is unit length (which is
    /// what `vec_nodes` and the 0.7 cosine thresholds assume) and identical
    /// texts get identical vectors. Nothing under test reads the *direction*:
    /// `insert_observation` stores what it is handed and does not dedup —
    /// that is `consolidate::store_observation`'s job, and the importer
    /// deliberately does not call it (legacy already consolidated these).
    fn stub(texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0.0f32; EMBEDDING_DIM];
                let slot = text.bytes().fold(0usize, |a, b| {
                    a.wrapping_mul(31).wrapping_add(b as usize) % EMBEDDING_DIM
                });
                v[slot] = 1.0;
                v
            })
            .collect())
    }

    struct Fixture {
        snapshot: Snapshot,
        _dir: tempfile::TempDir,
        db: std::path::PathBuf,
        cfg: Config,
    }

    impl Fixture {
        fn new(snapshot: Snapshot) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            Fixture {
                snapshot,
                db: dir.path().join("import.db"),
                _dir: dir,
                cfg: Config::defaults().unwrap(),
            }
        }

        fn real() -> Fixture {
            Fixture::new(Snapshot::real())
        }

        fn db_path(&self) -> &std::path::Path {
            &self.db
        }

        fn options(&self) -> Options<'_> {
            Options {
                snapshot: self.snapshot.path(),
                db: &self.db,
                replace: false,
                cfg: &self.cfg,
                embed: &stub,
                // `None` on purpose: the fact backlog needs the real model,
                // and `drain_backlog`'s own bound is covered directly by
                // `an_embedder_that_never_loads_fails_the_run_rather_than_spinning`.
                drain: None,
            }
        }

        async fn import(&self) -> Result<Vec<BankReport>> {
            run(&self.options()).await
        }

        async fn import_replacing(&self) -> Result<Vec<BankReport>> {
            run(&Options {
                replace: true,
                ..self.options()
            })
            .await
        }

        fn open(&self) -> Db {
            Db::open(&self.db).unwrap()
        }
    }

    fn count(db: &Db, sql: &str) -> i64 {
        db.read().unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn strings(db: &Db, sql: &str) -> Vec<String> {
        let conn = db.read().unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<String>>>().unwrap()
    }

    // --- the whole of `run()`, which D1 had no coverage for at all ---------

    /// The end-to-end shape, against the committed `real/` slice. D1's 41
    /// tests all target pure functions, which is exactly how a
    /// one-directional reconciliation reached review instead of a red test.
    #[tokio::test]
    async fn a_real_slice_imports_with_every_count_matching_its_oracle() {
        let fixture = Fixture::real();
        let reports = fixture.import().await.expect("the committed slice imports");
        let report = &reports[0];
        assert_eq!(report.bank_id, JCODE);
        assert_eq!(
            (report.documents, report.facts, report.observations),
            (1, 86, 79)
        );
        assert_eq!(
            report.facts as i64 + report.observations as i64,
            report.legacy_nodes
        );
        assert_eq!(report.causal_links as i64, report.legacy_caused_by);
        assert_eq!(report.documents as i64, report.legacy_documents);

        let db = fixture.open();
        assert_eq!(count(&db, "SELECT count(*) FROM documents"), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM memory_nodes"), 165);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE fact_type='observation'"
            ),
            79
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM links WHERE link_type='caused_by'"
            ),
            4
        );
        // Nothing but `caused_by` is copied: `semantic` needs the backlog
        // worker (deferred here) and `entity` rows are written by neither
        // system (`links.rs:6-8`, `counts.py:47-49`).
        assert_eq!(
            count(&db, "SELECT count(*) FROM links WHERE link_type='entity'"),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE document_id IS NULL AND fact_type<>'observation'"
            ),
            0,
            "every fact hangs off its document"
        );
    }

    /// The migration's join key, which MG-2's 50-sample diff is the only
    /// consumer of. `text` alone collides 101 times across the corpus, so a
    /// key that is merely *present* is not enough — it has to be unique.
    #[tokio::test]
    async fn every_fact_carries_a_unique_legacy_key() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes
                 WHERE fact_type <> 'observation'
                   AND json_extract(metadata, '$.legacy.document_id') IS NOT NULL
                   AND json_extract(metadata, '$.legacy.fact_index') IS NOT NULL"
            ),
            86
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM (
                   SELECT DISTINCT json_extract(metadata, '$.legacy.document_id'),
                                   json_extract(metadata, '$.legacy.fact_index')
                   FROM memory_nodes WHERE fact_type <> 'observation')"
            ),
            86
        );
        // The archive's own `metadata` survives beside it.
        assert!(
            strings(
                &db,
                "SELECT metadata FROM memory_nodes WHERE fact_type='world' LIMIT 1"
            )[0]
            .contains("session_id")
        );
    }

    /// `causal_relations` point **both** ways inside one document — 2 forward
    /// and 2 backward in this slice — and a mapping that assumed forward-only
    /// would still produce four edges, just wrong ones.
    #[tokio::test]
    async fn causal_links_land_on_the_pair_the_archive_names_including_backwards() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        let edges: Vec<(i64, i64)> = {
            let conn = db.read().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT json_extract(f.metadata,'$.legacy.fact_index'),
                            json_extract(t.metadata,'$.legacy.fact_index')
                     FROM links l
                     JOIN memory_nodes f ON f.id = l.from_node_id
                     JOIN memory_nodes t ON t.id = l.to_node_id
                     WHERE l.link_type='caused_by' ORDER BY 1",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        // `real/README.md`: 130->147, 155->160, 198->166, 204->191, shifted by
        // -125 when the slice was cut.
        assert_eq!(edges, vec![(5, 22), (30, 35), (73, 41), (79, 66)]);
        let backwards = edges.iter().filter(|(from, to)| to < from).count();
        assert_eq!(
            backwards, 2,
            "a forward-only mapping would pass without this"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM links WHERE link_type='caused_by' AND weight=1.0"
            ),
            4,
            "CAUSAL_LINK_WEIGHT, causal_links.py:18"
        );
    }

    /// `event_date = occurred_start.or(mentioned_at)` (`writes.py:80` parity).
    /// 78 of this slice's 86 facts fall through to `mentioned_at`, which is
    /// the majority shape across the whole corpus.
    #[tokio::test]
    async fn event_date_is_derived_not_copied() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes
                 WHERE fact_type <> 'observation'
                   AND event_date IS NOT coalesce(occurred_start, mentioned_at)"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes
                 WHERE fact_type <> 'observation' AND occurred_start IS NULL
                   AND event_date = mentioned_at"
            ),
            78
        );
    }

    /// Tags are gated as a multiset by MG-2, and this slice's list is the one
    /// a generator would not invent: `file:` tags beside a bare document uuid,
    /// repeated on the document, every fact and every observation.
    #[tokio::test]
    async fn tags_survive_on_facts_and_on_observations() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(&db, "SELECT count(DISTINCT node_id) FROM node_tags"),
            165,
            "every fact and every observation carries the document's tag list"
        );
        assert!(
            count(
                &db,
                "SELECT count(*) FROM node_tags t
                 JOIN memory_nodes n ON n.id = t.node_id
                 WHERE n.fact_type='observation' AND t.tag LIKE 'file:%'"
            ) > 0,
            "observation tags have no store helper on the insert path and are \
             the easiest thing in this module to drop"
        );
    }

    /// Entity names arrive **normalized and otherwise untouched**.
    ///
    /// Legacy's canonical names are not lowercased (`Agent`, `BM25`,
    /// `Claude`) and `write_entities` upserts on `(bank_id, canonical_name)`,
    /// so carrying them raw would split every entity in two the first time the
    /// daemon retained after cutover. Going the other way and running
    /// `entities::resolve_fact`'s fuzzy pass over them dissolved 77 of 3,917
    /// names into other entities — so the count assertion below is exact, and
    /// it is the one that fails if either half comes back.
    #[tokio::test]
    async fn entities_are_normalized_and_nothing_else() {
        let fixture = Fixture::real();
        let reports = fixture.import().await.unwrap();
        let db = fixture.open();

        // Every distinct normalized name in the slice gets its own row, and
        // every mention its own `node_entities` edge. Both are exact — an
        // entity graph that merely "looks reasonable" is what let 77 wrong
        // merges through the first time.
        let archive = crate::migrate::archive::load_dir(fixture.snapshot.path()).unwrap();
        let mut names: std::collections::BTreeSet<String> = Default::default();
        let mut mentions = 0i64;
        for fact in archive[0].documents.iter().flat_map(|d| d.facts.iter()) {
            let normalized = entities::normalized_mentions(&fact.entities);
            mentions += normalized.len() as i64;
            names.extend(normalized);
        }
        // The `> 0` guards the first version of this test dropped. `real/` is
        // a *redacted* slice and the redaction pass is exactly what would
        // strip an `entities` array, at which point every equality below
        // becomes `0 == 0` and the test goes green over an empty entity graph
        // — the shape this repo's review gate keeps naming: the equality
        // catches disagreement, the `> 0` catches disappearance.
        assert!(
            mentions > 0 && !names.is_empty(),
            "the slice names entities"
        );
        assert_eq!(reports[0].entities, names.len());
        assert_eq!(
            count(&db, "SELECT count(*) FROM entities"),
            names.len() as i64
        );
        assert_eq!(count(&db, "SELECT count(*) FROM node_entities"), mentions);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM entities WHERE canonical_name <> lower(canonical_name)"
            ),
            0,
            "an unnormalized name is one the daemon's own resolver can never match"
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM entities WHERE mention_count < 1"),
            0
        );
        // first_seen comes from the fact's own date, not a bank-wide stamp
        // (`entity_processing.py:28`).
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM entities WHERE first_seen IS NULL"
            ),
            0
        );
    }

    /// `insert_observation` writes none of the four date columns
    /// (`consolidate.rs:139-155`) and stamps `mentioned_at = now`, so every
    /// assertion here is about the second write in `write_observations`.
    ///
    /// The temporal-edge assertion is **not** about that write — step 7 builds
    /// its `TimedNode`s from the archive, so the edges land either way. It is
    /// here because D3's Tier-1 self-consistency check re-runs our own rule
    /// over the *migrated nodes'* `event_date`, and that is the check the
    /// stamp makes possible.
    #[tokio::test]
    async fn observations_get_their_dates_their_key_and_their_temporal_edges() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE fact_type='observation' AND event_date IS NULL"
            ),
            0
        );
        // Falsifiable, because the stamp is unconditional: an archive
        // `mentioned_at: null` now lands as NULL rather than as the migration
        // wall clock, so this counts the archive's own values.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE fact_type='observation'
                 AND mentioned_at IS NULL"
            ),
            0
        );
        // §Binding decisions #4's observation identity, which the first
        // version of this module did not write at all.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE fact_type='observation'
                 AND json_array_length(json_extract(metadata,'$.legacy.observation_of')) >= 1"
            ),
            79
        );
        let linked = count(
            &db,
            "SELECT count(DISTINCT l.from_node_id) FROM links l
             JOIN memory_nodes n ON n.id = l.from_node_id
             WHERE l.link_type='temporal' AND n.fact_type='observation'",
        );
        assert_eq!(
            linked, 79,
            "every observation has at least one temporal edge"
        );
        // And they only ever reach other observations: `temporal_links` pairs
        // same-`fact_type` (`links.rs:66-67`).
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM links l
                 JOIN memory_nodes f ON f.id = l.from_node_id
                 JOIN memory_nodes t ON t.id = l.to_node_id
                 WHERE l.link_type='temporal' AND f.fact_type <> t.fact_type"
            ),
            0
        );
    }

    /// **2,114, not 2,200.** `link_sources_tx` is `INSERT OR IGNORE` against
    /// the `(observation_id, source_id)` PK (`consolidate.rs:638-650`), so a
    /// duplicate `(document_id, fact_index)` pair collapses — and `proof_count`
    /// is recounted from what survived rather than carried.
    ///
    /// Only `real-cms/` can show this: all 86 duplicates in the live corpus
    /// are in `claude-code::bank-b`, and `real/`'s bank has zero.
    #[tokio::test]
    async fn duplicate_source_pairs_collapse_and_proof_count_follows() {
        let fixture = Fixture::new(Snapshot::real_cms());
        let reports = fixture.import().await.unwrap();
        // 114 raw source references in the slice, 68 distinct.
        assert_eq!(reports[0].node_sources, 68);
        let db = fixture.open();
        assert_eq!(count(&db, "SELECT count(*) FROM node_sources"), 68);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes n
                 WHERE n.fact_type='observation'
                   AND n.proof_count <> (SELECT count(*) FROM node_sources s
                                         WHERE s.observation_id = n.id)"
            ),
            0,
            "proof_count is derived from node_sources, never carried"
        );
        // Legacy's stored `proof_count` disagrees with ours in 43 of the 65 —
        // by construction (`export.py:457`'s `or len(source_ids)` fallback),
        // which is why MG-2 reports it and does not gate on it.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE fact_type='observation' AND proof_count=1"
            ),
            64
        );
    }

    /// Multi-bank driving, from the two committed real fixtures composed into
    /// one snapshot directory.
    #[tokio::test]
    async fn two_banks_import_side_by_side_without_leaking_into_each_other() {
        let fixture = Fixture::new(Snapshot::both());
        let reports = fixture.import().await.unwrap();
        assert_eq!(reports.len(), 2);
        let db = fixture.open();
        assert_eq!(count(&db, "SELECT count(*) FROM banks"), 2);
        assert_eq!(
            count(
                &db,
                &format!("SELECT count(*) FROM memory_nodes WHERE bank_id='{JCODE}'")
            ),
            165
        );
        assert_eq!(
            count(
                &db,
                &format!("SELECT count(*) FROM memory_nodes WHERE bank_id='{CMS}'")
            ),
            135
        );
        // No link may cross a bank: `temporal_links` is called once per bank
        // over that bank's nodes only.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM links l
                 JOIN memory_nodes f ON f.id=l.from_node_id
                 JOIN memory_nodes t ON t.id=l.to_node_id
                 WHERE f.bank_id <> t.bank_id"
            ),
            0
        );
        // Entities are scoped by `(bank_id, canonical_name)`, which is the key
        // the whole of step 4 rests on — and the two slices genuinely overlap:
        // 8 normalized names appear in both (`bm25`, `recall`, `llm`,
        // `prompt`, `master`, `assistant`, `tool_use`, `tool_result`). A
        // regression dropping the `bank_id` predicate collapses 369 rows to
        // 361 and nothing else in the suite would notice.
        let per_bank = |bank: &str| {
            count(
                &db,
                &format!("SELECT count(*) FROM entities WHERE bank_id='{bank}'"),
            )
        };
        assert_eq!((per_bank(JCODE), per_bank(CMS)), (203, 166));
        assert_eq!(count(&db, "SELECT count(*) FROM entities"), 369);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM node_entities ne
                 JOIN memory_nodes n ON n.id = ne.node_id
                 JOIN entities e ON e.id = ne.entity_id
                 WHERE n.bank_id <> e.bank_id"
            ),
            0,
            "no mention may attach to another bank's entity"
        );

        // Each bank's watermark is its own MAX(id), not the database's.
        let marks: Vec<i64> = {
            let conn = db.read().unwrap();
            let mut stmt = conn
                .prepare("SELECT watermark FROM consolidation_runs ORDER BY bank_id")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(marks.len(), 2);
        assert!(marks[0] < marks[1]);
    }

    /// Open question 10 in the plan: *"the marker is a design in this plan,
    /// not a mechanism I ran"*. This is the round trip, and it also pins that
    /// legacy's own disposition survives beside it — `banks.disposition` has a
    /// `json_valid` CHECK (`0001_init.sql:13`) and nothing else writes a
    /// nested object into it today.
    #[tokio::test]
    async fn the_marker_round_trips_beside_the_legacy_disposition() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        let db = fixture.open();
        let disposition = banks::get(&db, JCODE)
            .unwrap()
            .unwrap()
            .disposition
            .unwrap();
        let value: Value = serde_json::from_str(&disposition).unwrap();
        assert_eq!(value["skepticism"], 3, "legacy's own disposition survives");
        assert_eq!(value[MARKER_KEY]["state"], "done");
        assert_eq!(
            value[MARKER_KEY]["snapshot"].as_str().unwrap().len(),
            64,
            "the snapshot's own hash, so verify can catch a bank imported from another one"
        );
        assert_eq!(
            banks::get(&db, JCODE).unwrap().unwrap().mission.unwrap(),
            "You are a coding assistant with long-term memory of this project's engineering \
             history: decisions, bug fixes, conventions, and workflows.",
            "banks.json is the only carrier of the mission; the archive has none"
        );
    }

    /// §Binding decisions #5b, both directions. Without the row the daemon
    /// re-consolidates the whole migrated corpus within one poll interval of
    /// restart; with a stale one after `--replace`, ids restart at 1 while the
    /// watermark does not and the front of the import is invisible forever.
    #[tokio::test]
    async fn the_watermark_row_covers_every_node_the_bank_now_holds() {
        let fixture = Fixture::real();
        let reports = fixture.import().await.unwrap();
        let db = fixture.open();
        let max_id = count(&db, "SELECT MAX(id) FROM memory_nodes");
        assert_eq!(reports[0].watermark, max_id);
        assert_eq!(consolidate::watermark(&db, JCODE).unwrap(), max_id);
        // Discriminating, unlike `count_unconsolidated(db, bank, max_id)`,
        // which is `id > MAX(id)` and is 0 for any database at all. The pair
        // is what shows the row did something: at watermark 0 the whole
        // imported corpus is due, at the written watermark none of it is.
        assert_eq!(
            consolidate::count_unconsolidated(&db, JCODE, 0).unwrap(),
            86,
            "without the row the daemon would re-consolidate every migrated fact"
        );
        assert_eq!(
            consolidate::count_unconsolidated(
                &db,
                JCODE,
                consolidate::watermark(&db, JCODE).unwrap()
            )
            .unwrap(),
            0,
            "with it, nothing the import wrote is due"
        );
    }

    // --- the guards --------------------------------------------------------

    #[tokio::test]
    async fn a_non_empty_bank_is_refused_without_replace() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::BankNotEmpty { nodes: 165, .. })
        ));
    }

    /// The partial-bank guarantee, and the reason it replaces the plan's first
    /// draft's `count(*) == 0` assertion: there is no per-bank transaction, so
    /// a failed run leaves rows and the *marker* is what makes them non-silent.
    ///
    /// The failure is injected at step 6 — after the marker, the documents,
    /// the facts and the causal links have all been written.
    #[tokio::test]
    async fn a_failure_mid_import_leaves_a_marked_bank_that_refuses_to_be_reused() {
        let fixture = Fixture::real();
        let broken = |_: &[String]| -> anyhow::Result<Vec<Vec<f32>>> {
            anyhow::bail!("the embedder fell over")
        };
        let failed = run(&Options {
            embed: &broken,
            ..fixture.options()
        })
        .await;
        assert!(matches!(failed, Err(MigrateError::Embed { .. })));

        let db = fixture.open();
        assert_eq!(
            count(&db, "SELECT count(*) FROM memory_nodes"),
            86,
            "the facts are on disk: there is no transaction to roll them back"
        );
        let disposition = banks::get(&db, JCODE)
            .unwrap()
            .unwrap()
            .disposition
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&disposition).unwrap()[MARKER_KEY]["state"],
            "running"
        );
        drop(db);

        // The marker is checked before the row count, and it is the half that
        // still fires when the failure happened *before* the first node — the
        // case `a_non_empty_bank_is_refused_without_replace` cannot reach.
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::ImportInProgress { .. })
        ));
        let db = fixture.open();
        db.write(|tx| {
            tx.execute("DELETE FROM memory_nodes WHERE bank_id = ?1", [JCODE])
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))
        })
        .unwrap();
        assert!(
            matches!(
                assert_bank_available(&db, JCODE, false),
                Err(MigrateError::ImportInProgress { .. })
            ),
            "a bank that failed before its first node has zero rows and is still not reusable"
        );
        drop(db);

        // And `--replace` is the way out, ending at `done`.
        let reports = fixture.import_replacing().await.unwrap();
        assert_eq!(reports[0].facts, 86);
        let db = fixture.open();
        assert_eq!(count(&db, "SELECT count(*) FROM memory_nodes"), 165);
    }

    /// `--replace` deletes what the migration owns and spares what it does
    /// not. The `retain_jobs` half is the one that is easy to get wrong: a job
    /// whose row vanishes resolves to a 404, which `cmd/retain.rs:498-504`
    /// reads as `Failed` and rolls the client cursor back — so deleting them
    /// causes re-ingestion rather than cleanliness.
    #[tokio::test]
    async fn replace_purges_the_bank_and_spares_retain_jobs() {
        let fixture = Fixture::real();
        fixture.import().await.unwrap();

        let db = fixture.open();
        let document_id: i64 = count(&db, "SELECT id FROM documents LIMIT 1");
        memgarden_store::sessions::upsert(
            &db,
            JCODE,
            &memgarden_store::sessions::SessionUpdate {
                session_id: "s-1",
                byte_offset: Some(4096),
                ..Default::default()
            },
        )
        .unwrap();
        memgarden_store::retain_jobs::insert(
            &db,
            "job-1",
            JCODE,
            Some(document_id),
            Some("s-1"),
            None,
            None,
        )
        .unwrap();
        memgarden_store::mental_models::insert(
            &db,
            &memgarden_store::mental_models::NewMentalModel {
                id: "mm-1",
                bank_id: JCODE,
                name: "m",
                source_query: None,
                content: "c",
                max_tokens: None,
                trigger: None,
            },
            None,
        )
        .unwrap();
        drop(db);

        fixture.import_replacing().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(&db, "SELECT count(*) FROM memory_nodes"),
            165,
            "exactly one copy"
        );
        assert_eq!(count(&db, "SELECT count(*) FROM documents"), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM sessions"), 0);
        assert_eq!(count(&db, "SELECT count(*) FROM mental_models"), 0);
        assert_eq!(
            count(&db, "SELECT count(*) FROM consolidation_runs"),
            1,
            "the stale watermark is gone and only the new run's row remains"
        );
        assert_eq!(count(&db, "SELECT count(*) FROM retain_jobs"), 1, "spared");
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM retain_jobs WHERE document_id IS NULL"
            ),
            1,
            "but its document join is severed by ON DELETE SET NULL, and that is \
             not free — the row is AC-2 evidence"
        );
    }

    /// Both halves of the live-daemon guard, and the point is that **neither
    /// alone refuses**. The plan states only the listener half, which would
    /// refuse every zero-downtime rehearsal the runbook asks for.
    #[tokio::test]
    async fn the_daemon_guard_needs_a_listener_and_the_same_database() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let held = tempfile::tempdir().unwrap();
        let held_db = held.path().join("live.db");
        std::fs::write(&held_db, b"").unwrap();
        let elsewhere = held.path().join("rehearsal.db");
        std::fs::write(&elsewhere, b"").unwrap();

        let mut cfg = Config::defaults().unwrap();
        cfg.bind = bind;
        cfg.db_path = held_db.clone();
        assert!(matches!(
            assert_daemon_not_holding(&cfg, &held_db),
            Err(MigrateError::DaemonListening { .. })
        ));
        assert!(
            assert_daemon_not_holding(&cfg, &elsewhere).is_ok(),
            "a rehearsal into another file is what the runbook does with the daemon up"
        );

        drop(listener);
        assert!(
            assert_daemon_not_holding(&cfg, &held_db).is_ok(),
            "nothing listening, so nothing to collide with"
        );
    }

    /// `Db::open` migrates forward and is silent about a database written by a
    /// newer binary: every entry sees `version <= current` and skips.
    #[tokio::test]
    async fn a_database_from_a_newer_binary_is_refused() {
        let fixture = Fixture::real();
        let db = fixture.open();
        db.write(|tx| {
            tx.pragma_update(None, "user_version", memgarden_store::LATEST_VERSION + 1)
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))
        })
        .unwrap();
        drop(db);
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::SchemaVersionMismatch { .. })
        ));
    }

    /// The checksum guard is in front of everything, so a snapshot whose bytes
    /// moved after it was frozen never reaches the database.
    #[tokio::test]
    async fn a_snapshot_whose_bytes_moved_is_refused_before_the_database_is_opened() {
        let fixture = Fixture::real();
        std::fs::write(
            fixture.snapshot.path().join("banks.json"),
            br#"{"banks":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::ChecksumMismatch { .. })
        ));
        assert!(
            !fixture.db_path().exists(),
            "the guard runs before `Db::open`, which would otherwise create and migrate a file"
        );
    }

    // --- the pre-write refusals D1 left to D2 ------------------------------

    /// `edge::legal-but-absent` carries `target_fact_index: 999` on purpose,
    /// and D1 accepts it on purpose: the range check is D2's, and it is
    /// **before** any write because `graph::insert_links` would otherwise fail
    /// on a foreign key naming neither the document nor the fact.
    #[tokio::test]
    async fn an_out_of_range_causal_target_is_refused_before_any_write() {
        let fixture = Fixture::new(Snapshot::edge(
            "edge__legal-but-absent",
            "edge::legal-but-absent",
        ));
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::CausalTargetOutOfRange {
                target: 999,
                facts: 3,
                ..
            })
        ));
        assert!(!fixture.db_path().exists());
    }

    /// The same fixture, with the out-of-range target repaired: `context: ""`
    /// is legal per `schema.py` and must import, as `NULL`.
    #[tokio::test]
    async fn an_empty_context_becomes_null_rather_than_an_empty_string() {
        let fixture = Fixture::new(Snapshot::edge(
            "edge__legal-but-absent",
            "edge::legal-but-absent",
        ));
        fixture
            .snapshot
            .edit("edge__legal-but-absent/documents/000000.json", |doc| {
                doc["facts"][1]["causal_relations"][0]["target_fact_index"] = json!(0)
            });
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(&db, "SELECT count(*) FROM memory_nodes WHERE context = ''"),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes WHERE context IS NULL AND fact_type <> 'observation'"
            ),
            3
        );
        // And the duplicate source pair in that fixture collapses: 3 raw
        // references, 2 distinct.
        assert_eq!(count(&db, "SELECT count(*) FROM node_sources"), 2);
        assert_eq!(
            count(
                &db,
                "SELECT proof_count FROM memory_nodes WHERE fact_type='observation'"
            ),
            2,
            "legacy stored 3; ours is derived from what survived"
        );
    }

    /// `insert_observation` filters unresolvable source ids in SQL and drops
    /// them **silently** (`consolidate.rs:111-114`) — right for the daemon,
    /// and a silent loss of proof here.
    #[tokio::test]
    async fn an_observation_source_that_names_no_fact_is_refused() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/observations.json", |obs| {
                obs[0]["sources"][0]["fact_index"] = json!(9999)
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::ObservationSourceUnresolved {
                fact_index: 9999,
                ..
            })
        ));
        assert!(!fixture.db_path().exists());
    }

    /// The one shape `deny_unknown_fields` structurally cannot see: not an
    /// unknown field *in* the archive, but a field the archive does not have.
    /// Step 2 carries `retain_params.metadata` as the document's metadata on
    /// the strength of an equality measured 25/25 — asserted here rather than
    /// assumed, because a drift lands metadata legacy does not associate with
    /// the document and nothing says so.
    #[tokio::test]
    async fn document_metadata_that_disagrees_with_retain_params_is_refused() {
        let fixture = Fixture::real();
        fixture.snapshot.edit("stats.json", |stats| {
            stats[JCODE]["documents"][0]["document_metadata"]["session_id"] = json!("someone-else");
        });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::DocumentMetadataMismatch { .. })
        ));
    }

    /// Legacy types `fact_type` as a free string; ours is a three-value
    /// `CHECK`. `recall_bench.rs:148` defaults an unrecognised value to
    /// `world`, which for a migration files a legacy shape change as ordinary
    /// content.
    #[tokio::test]
    async fn an_unrecognised_fact_type_is_refused_rather_than_defaulted() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"][0]["fact_type"] = json!("belief")
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::UnknownFactType { .. })
        ));
    }

    /// D1 refuses a non-null `observation_scopes` on an observation and its
    /// deferred list records that the same field on a **fact** is unchecked.
    /// It is the same silent drop — no column, and a known-but-unused field is
    /// what `deny_unknown_fields` structurally cannot catch.
    #[tokio::test]
    async fn a_fact_carrying_observation_scopes_is_refused() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"][0]["observation_scopes"] = json!("per_tag")
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::FactScopesUnsupported { fact_index: 0, .. })
        ));
        assert!(!fixture.db_path().exists());
    }

    /// *"There is nothing to migrate"* is a claim, and `--replace` makes being
    /// wrong about it concrete: it deletes the target bank's `mental_models`.
    #[tokio::test]
    async fn a_manifest_that_claims_content_this_importer_drops_is_refused() {
        for field in ["mental_model_count", "directive_count", "webhook_count"] {
            let fixture = Fixture::real();
            fixture
                .snapshot
                .edit("claude-code__bank-a/manifest.json", |m| {
                    m[field] = json!(1)
                });
            assert!(
                matches!(
                    fixture.import().await,
                    Err(MigrateError::UnsupportedArchiveContent { .. })
                ),
                "{field} was accepted"
            );
        }
    }

    /// `includes_history: true` means the archive carries an edit history no
    /// column here can hold, and `schema.py` does not say in which files.
    #[tokio::test]
    async fn an_archive_that_claims_to_include_history_is_refused() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/manifest.json", |m| {
                m["includes_history"] = json!(true)
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::UnsupportedArchiveContent {
                field: "includes_history",
                ..
            })
        ));
    }

    /// A `"bank"` archive carries those three in files `load_dir` never opens.
    #[tokio::test]
    async fn a_bank_archive_is_refused_because_this_loader_reads_only_documents() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/manifest.json", |m| {
                m["archive_type"] = json!("bank")
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::UnsupportedArchiveType { .. })
        ));
    }

    /// Every field of the archive either lands in a column, lands in
    /// `metadata`, or is refused. `created_at` and `consolidation_failed_at`
    /// are the two that would otherwise vanish between the archive and the
    /// schema without anything saying so — the second because §Binding
    /// decisions #5b collapses the consolidation lifecycle into a single
    /// watermark rowid, which cannot express "this one failed".
    #[tokio::test]
    async fn the_legacy_key_carries_the_fields_no_column_can_hold() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"][0]["consolidation_failed_at"] = json!("2026-08-03T17:00:00Z")
            });
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes
                 WHERE fact_type <> 'observation'
                   AND json_extract(metadata, '$.legacy.created_at') IS NOT NULL"
            ),
            86
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM memory_nodes
                 WHERE json_extract(metadata, '$.legacy.consolidation_failed_at') IS NOT NULL"
            ),
            1,
            "carried only when set, so 3,540 nodes do not each hold a null"
        );
        // And `documents.created_at` gets the same treatment, for the same
        // reason: `documents::upsert` writes `now_ms()` (`documents.rs:73-77`)
        // and a migration does not reshape a store helper retain depends on.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM documents
                 WHERE json_extract(metadata, '$.legacy_created_at') IS NOT NULL"
            ),
            1
        );
    }

    /// A timestamp we cannot read costs the node its place in the temporal
    /// graph. `recall_bench.rs:186-191` returns `None`; a migration refuses.
    #[tokio::test]
    async fn a_timestamp_we_cannot_parse_is_refused_rather_than_dropped() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"][0]["mentioned_at"] = json!("yesterday")
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::BadTimestamp {
                field: "mentioned_at",
                ..
            })
        ));
    }

    /// A bank that reached the snapshot empty is not migrated, for the same
    /// reason a `--drop-bank` bank is not. `claude-code::memgarden`
    /// appeared in legacy between D1's snapshot and D2's with exactly this
    /// shape.
    #[tokio::test]
    async fn an_empty_archive_is_reported_and_not_turned_into_a_bank_row() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"] = json!([])
            });
        fixture
            .snapshot
            .edit("claude-code__bank-a/manifest.json", |m| {
                m["fact_count"] = json!(0);
                m["observation_count"] = json!(0);
                m["document_count"] = json!(0);
            });
        fixture
            .snapshot
            .edit("claude-code__bank-a/observations.json", |o| {
                *o = json!([])
            });
        std::fs::remove_file(
            fixture
                .snapshot
                .path()
                .join("claude-code__bank-a/documents/000000.json"),
        )
        .unwrap();
        fixture.snapshot.edit("stats.json", |stats| {
            stats[JCODE]["stats"]["total_nodes"] = json!(0);
            stats[JCODE]["stats"]["total_documents"] = json!(0);
            stats[JCODE]["stats"]["links_by_link_type"]["caused_by"] = json!(0);
            stats[JCODE]["documents"] = json!([]);
            stats[JCODE]["documents_total"] = json!(0);
            stats[JCODE]["memories_total"] = json!(0);
        });

        let reports = fixture.import().await.unwrap();
        assert!(reports[0].skipped_empty);
        assert!(reports[0].line().starts_with("skip "));
        let db = fixture.open();
        assert_eq!(
            count(&db, "SELECT count(*) FROM banks"),
            0,
            "an empty bank reappears on first use via hook session-start's POST /v1/banks"
        );
    }

    /// The cross-document half of §1's trade, which no real fixture can show:
    /// `real/` and `real-cms/` are both single-document, so
    /// `load_resolution_context` hands `resolve_fact` an empty candidate list
    /// and the resolver degenerates to `normalize`.
    ///
    /// Both directions matter. `BM25`/`bm25` and `Recall`/`recall` must
    /// **collapse** — that is the whole reason step 4 follows retain's path
    /// rather than the bench's. `Postgres`/`SQLite` must **stay apart** — the
    /// cost of running a fuzzy resolver over names legacy already
    /// canonicalized is that it can merge two it kept separate, and this is
    /// the assertion that fails if that starts happening.
    #[tokio::test]
    async fn entity_names_collapse_across_documents_by_case_and_not_by_accident() {
        let fixture = Fixture::new(Snapshot::edge("edge__two-documents", "edge::two-documents"));
        fixture.import().await.unwrap();
        let db = fixture.open();
        let names = strings(
            &db,
            "SELECT canonical_name FROM entities ORDER BY canonical_name",
        );
        assert_eq!(
            names,
            vec!["bm25", "postgres", "recall", "reranker", "sqlite"],
            "7 mentions of 6 distinct raw names across 2 documents land as 5 \
             entities: only the case variants merged"
        );
        // The merged one is mentioned in both documents, and `write_entities`
        // counts per occurrence (`entity_resolver.py:718`).
        assert_eq!(
            count(
                &db,
                "SELECT mention_count FROM entities WHERE canonical_name='bm25'"
            ),
            2
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM node_entities"),
            7,
            "all seven mentions still attach to their own node; only the names merged"
        );

        // Joined back to a *specific* node, because the aggregate counts above
        // are all invariant under a mis-zipped `(fact, draft, id)` — review
        // checked, and reversing `fact_ids` leaves every one of them
        // unchanged. This is the only assertion that catches it.
        let of_second_fact = strings(
            &db,
            "SELECT e.canonical_name FROM entities e
             JOIN node_entities ne ON ne.entity_id = e.id
             JOIN memory_nodes n ON n.id = ne.node_id
             WHERE n.text LIKE 'Postgres is not SQLite%' ORDER BY 1",
        );
        assert_eq!(of_second_fact, vec!["postgres", "sqlite"]);

        // And the co-occurrence pairs, which nothing in the migration path
        // asserted at all — a change that emitted none would have been silent.
        // Two pairs: (bm25, recall) on both documents' first facts, and
        // (postgres, sqlite) on one.
        let pairs = strings(
            &db,
            "SELECT a.canonical_name || '+' || b.canonical_name || '=' || c.cooccurrence_count
             FROM entity_cooccurrences c
             JOIN entities a ON a.id = c.entity_id_1
             JOIN entities b ON b.id = c.entity_id_2 ORDER BY 1",
        );
        assert_eq!(pairs, vec!["bm25+recall=2", "postgres+sqlite=1"]);
    }

    /// **The fixture that discriminates**, and the one this PR did not have
    /// until review pointed out that reverting the fix left the suite green.
    ///
    /// `edge::fuzzy-merge-bait` is two documents sharing a date, one naming
    /// `CE-1` and `Phase A`, the other `CE-4` and `Phase A`. Under
    /// `entities::resolve_fact`, `ce-4` scores
    /// `ratio("ce-4","ce-1")*0.5 (= 0.375) + overlap 1/1 * 0.3 + same-day 0.2
    /// = 0.875` against the 0.6 gate and collapses into `ce-1` — the live
    /// corpus's headline failure, in three facts. Normalizing and stopping
    /// keeps them apart.
    #[tokio::test]
    async fn two_similar_names_sharing_a_partner_and_a_day_stay_apart() {
        let fixture = Fixture::new(Snapshot::edge(
            "edge__fuzzy-merge-bait",
            "edge::fuzzy-merge-bait",
        ));
        fixture.import().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            strings(&db, "SELECT canonical_name FROM entities ORDER BY 1"),
            vec!["ce-1", "ce-4", "phase a"],
            "the fuzzy pass merges ce-4 into ce-1 here; normalization does not"
        );
        assert_eq!(count(&db, "SELECT count(*) FROM node_entities"), 4);
    }

    /// Legacy's `event_date` is NOT NULL as exactly this case's fallback
    /// (`schema.py:57-58`); ours is derived and can be NULL, and a NULL
    /// `event_date` is skipped by `temporal_links`. Measured 0 today, asserted
    /// for the same reason `original_text`'s 25/25 is.
    #[tokio::test]
    async fn a_fact_whose_event_date_cannot_be_derived_is_refused() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                doc["facts"][0]["occurred_start"] = json!(null);
                doc["facts"][0]["mentioned_at"] = json!(null);
            });
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::EventDateNotDerivable { index: 0, .. })
        ));
        assert!(!fixture.db_path().exists());
    }

    /// `banks.json` is the only carrier of a mission, so an archive with no
    /// entry in it is the mirror of `StatsMissing` — and silence there means a
    /// bank created with a NULL mission and nothing saying the string was
    /// lost.
    #[tokio::test]
    async fn an_archive_with_no_banks_json_entry_is_refused() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("banks.json", |b| b["banks"] = json!([]));
        assert!(matches!(
            fixture.import().await,
            Err(MigrateError::BankNotListed { .. })
        ));
    }

    /// `banks::update` reads `None` as "leave it" and `Some(None)` as "set
    /// NULL" (`banks.rs:56-59`). The bank that already exists on :9100 was
    /// created by `hook session-start` with a mission of its own, so passing
    /// an absent legacy mission straight through would erase it.
    #[tokio::test]
    async fn replace_does_not_erase_the_mission_of_a_bank_legacy_has_none_for() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("banks.json", |b| b["banks"][0]["mission"] = json!(null));
        let db = fixture.open();
        banks::create(&db, JCODE, Some("set by hook session-start"), None).unwrap();
        drop(db);

        fixture.import_replacing().await.unwrap();
        let db = fixture.open();
        assert_eq!(
            banks::get(&db, JCODE).unwrap().unwrap().mission.unwrap(),
            "set by hook session-start"
        );
    }

    /// The report line is the AC-3 evidence artifact, and its `==` used to be
    /// text rather than a comparison: `ok` printed unconditionally for any
    /// non-empty bank. A self-causal relation is the cheapest input that makes
    /// the two sides differ — `assert_integrity` counts it (archive == /stats)
    /// and `import_bank` drops it as a self-loop.
    #[tokio::test]
    async fn a_dropped_causal_edge_shows_up_as_mismatch_rather_than_ok() {
        let fixture = Fixture::real();
        fixture
            .snapshot
            .edit("claude-code__bank-a/documents/000000.json", |doc| {
                // Point fact 5's relation at itself; the archive count is
                // unchanged, so every D1 assertion still passes.
                doc["facts"][5]["causal_relations"][0]["target_fact_index"] = json!(5)
            });
        let reports = fixture.import().await.unwrap();
        assert_eq!(reports[0].causal_links, 3);
        assert!(!reports[0].reconciles());
        let line = reports[0].line();
        assert!(line.starts_with("MISMATCH"), "{line}");
        assert!(line.contains("causal 3 != 4"), "{line}");
    }

    // --- step 8's bound ----------------------------------------------------

    /// `drain_once` returns immediately when the model has not loaded
    /// (`embed_task.rs:80-82`), which from out here is indistinguishable from
    /// a drained backlog. Without the shrink check this is an infinite loop
    /// against an embedder that will never load.
    #[tokio::test]
    async fn an_embedder_that_never_loads_fails_the_run_rather_than_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(dir.path().join("d.db")).unwrap());
        banks::create(&db, "b", None, None).unwrap();
        nodes::insert(
            &db,
            NewNode::new("b", FactType::World, "a fact with no vector yet"),
        )
        .unwrap();

        // The state the daemon is in for the whole of a cold start. Built by
        // hand rather than through `backlog_state`, which wants a loaded
        // `Embedder` and would pull 133 MB of ONNX into a unit test.
        let cfg = Config::defaults().unwrap();
        let (retain_tx, _rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
        let state = AppState {
            db: db.clone(),
            ollama: Arc::new(crate::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap()),
            cfg: Arc::new(cfg),
            started_at_ms: now(),
            embedder: Arc::new(std::sync::RwLock::new(None)),
            reranker: Default::default(),
            consolidating: Default::default(),
            refreshing: Default::default(),
            retain_tx,
        };

        assert!(matches!(
            drain_backlog(&db, &state, "b").await,
            Err(MigrateError::EmbeddingBacklogStalled {
                pending: 1,
                calls: 1,
                ..
            })
        ));
    }

    // --- the whole corpus, with the real model -----------------------------

    /// The full four-bank import with the production embedder, which is what
    /// the manual verification runs. `#[ignore]`d for B1's reason: 133 MB of
    /// ONNX in CI is not a unit test.
    ///
    /// ```text
    /// cargo test -p memgardend --lib migrate::import -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "loads the 133MB embedding model and needs a real snapshot in MG_SNAPSHOT"]
    async fn the_real_snapshot_imports_end_to_end() {
        let Ok(snapshot) = std::env::var("MG_SNAPSHOT") else {
            panic!("set MG_SNAPSHOT to a `mg_migrate snapshot --out` directory");
        };
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::defaults().unwrap();
        let embedding = cfg.embedding.clone();
        let embedder = Arc::new(crate::embed::Embedder::load(&embedding).unwrap());
        let reports = run(&Options {
            snapshot: std::path::Path::new(&snapshot),
            db: &dir.path().join("import.db"),
            replace: false,
            cfg: &cfg,
            embed: &|texts| embedder.embed_batch(texts),
            drain: Some(embedder.clone()),
        })
        .await
        .unwrap();
        for report in &reports {
            println!("{}", report.line());
            assert!(report.skipped_empty || report.pending_embeddings == 0);
        }
    }
}
