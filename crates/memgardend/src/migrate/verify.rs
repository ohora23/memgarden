//! `mg-migrate verify` — MG-2, the instrument AC-3 is read from.
//!
//! # Why this is a separate binary path from `import`
//!
//! Counts printed by the program that wrote the rows are not evidence that the
//! rows are right. `import` reports what it *believes* it did; `verify` reads
//! three oracles that do not include the importer's opinion:
//!
//! | oracle | answers | provenance |
//! |---|---|---|
//! | `snapshot/stats.json` | *"how many did legacy have?"* | `GET /stats`, frozen at snapshot time |
//! | `snapshot/<bank>/…json` | *"what did legacy say?"* | `document-transfer`, frozen at snapshot time |
//! | the SQLite file | *"what do we have?"* | read through `memgarden_store` |
//!
//! A disagreement between the first two is a **snapshot integrity** failure,
//! reported separately from a migration failure: the first means the legacy
//! bank moved under the read, the second means we lost something.
//!
//! # Three tiers, because a single boolean would either lie or fail forever
//!
//! * **Tier 1 — equality.** Everything that *can* be equal: documents, nodes
//!   and their `fact_type` breakdown, observations, authored `caused_by`
//!   edges, observation provenance, tags, entities, and the post-conditions
//!   only we can state (the import marker, the consolidation watermark,
//!   embedding coverage, temporal self-consistency). Any mismatch is
//!   [`Verdict::Fail`].
//! * **Tier 2 — recomputed.** `temporal` and `semantic` are rebuilt from the
//!   migrated facts by our own rules, so they are *reported against a measured
//!   band*, never asserted equal. Outside the band is [`Verdict::Review`], not
//!   a failure.
//! * **Tier 3 — not applicable.** `entity` links, with the citation, so nobody
//!   re-litigates it from `/stats` output.
//!
//! # What Phase D measured that changes the tiers from the plan's shape
//!
//! * **Legacy stores zero derived edges on observations.** Decomposing
//!   `/graph` by endpoint `fact_type` gives temporal `3,269 == /stats 3,269`
//!   and semantic `4,603 == 4,603` for `bank-a`; every
//!   observation-touching edge there is a visualization copy
//!   (`memory_engine.py:7723-7724`). So the temporal band is compared
//!   **fact-to-fact only**, and our observation-to-observation edges are a new
//!   class reported without a ratio. A band on the total would pass a run in
//!   which the fact-edge rule silently broke.
//! * **`semantic` gets no band at all**, and not for the plan's reason ("no
//!   prior"). There is a prior — legacy's 65,127 — and we are at 6,890 because
//!   `embed_task.rs:178-179` confines every semantic edge to one 8-node
//!   embedding batch. Banding a number that a one-line CE-7 fix moves by 10×
//!   would be banding a bug.
//! * **Entities are a Tier-1 equality**, which the plan did not expect. MG-1b
//!   normalizes and stops, so the archive's distinct normalized names and its
//!   mention count are both exact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use memgarden_store::Db;
use serde::{Deserialize, Serialize};

use super::archive::{BankArchive, TransferFact, TransferObservation};
use super::snapshot::{self, Stats, sha256_hex};
use super::{MigrateError, Result, load_stats};
use crate::links::{self, TimedNode};

/// `temporal` ratio band, **fact-to-fact against legacy's fact-to-fact**.
///
/// Centred on 1.58-1.61, measured three ways over the archive by replaying
/// `links.rs:62-92` (whole-corpus 70,192, by `chunk_index` 68,781, by
/// `created_at` batch 69,771). MG-1b's own run lands at 70,212 / 43,657 =
/// **1.61**. Wide enough for corpus growth; anything outside means the rule
/// changed on one side.
pub const TEMPORAL_BAND: (f64, f64) = (1.45, 1.75);

/// `entity` links, Tier 3. Legacy derives its `/stats` number at read time
/// from `unit_entities` and stores no rows; `links.rs:6-8` is our matching
/// decision. Printed with the number so nobody re-derives it from `/stats`.
const ENTITY_CITATION: &str = "engine/memories/pg/counts.py:47-49 — derived at /stats time, \
                               not stored; links.rs:6-8 is our matching decision";

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Pass,
    Review,
    Fail,
}

impl Verdict {
    /// The contract the runbook and CI read. Kept next to the enum so the two
    /// cannot drift.
    pub fn exit_code(self) -> u8 {
        match self {
            Verdict::Pass => 0,
            Verdict::Fail => 1,
            Verdict::Review => 2,
        }
    }
}

/// One Tier-1 equality.
///
/// `source` names where `expected` came from, because three different things
/// produce one: legacy's frozen `/stats`, the frozen archive, and our own rule
/// run as a reference implementation. A column headed "legacy" for a check
/// legacy has no opinion about would be the dishonest version of this table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier1Check {
    pub name: String,
    pub expected: i64,
    pub actual: i64,
    pub source: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Tier1Check {
    fn new(name: impl Into<String>, source: &str, expected: i64, actual: i64) -> Self {
        Tier1Check {
            name: name.into(),
            expected,
            actual,
            source: source.to_string(),
            ok: expected == actual,
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// A recomputed adjacency count. Never an equality — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier2Metric {
    pub name: String,
    /// `None` when legacy has no counterpart at all, which is the honest
    /// answer for observation-to-observation edges.
    pub legacy: Option<i64>,
    pub ours: i64,
    pub ratio: Option<f64>,
    pub band: Option<(f64, f64)>,
    /// `true` when there is no band, or the ratio is inside it. A metric
    /// without a band cannot fail; it can only be read.
    pub ok: bool,
    pub note: String,
    /// What actually shows a rule change: a cap that stopped firing reads as a
    /// count drift but is unmistakable as a degree histogram collapsing.
    pub out_degree: Degrees,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Degrees {
    pub nodes: i64,
    pub mean: f64,
    pub p50: i64,
    pub p90: i64,
    pub max: i64,
}

impl Degrees {
    fn of(mut counts: Vec<i64>) -> Degrees {
        if counts.is_empty() {
            return Degrees::default();
        }
        counts.sort_unstable();
        let n = counts.len();
        Degrees {
            nodes: n as i64,
            mean: counts.iter().sum::<i64>() as f64 / n as f64,
            p50: counts[n / 2],
            p90: counts[(n * 9 / 10).min(n - 1)],
            max: counts[n - 1],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub bank: String,
    /// `<document uuid>#<fact_index>` for a fact, `observation:<n>` for an
    /// observation — the same key the sample was drawn on.
    pub key: String,
    pub field: String,
    pub legacy: String,
    pub ours: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleReport {
    pub n: usize,
    pub seed: u64,
    pub drawn: usize,
    /// Stratified by bank in proportion to node count, so the largest bank
    /// contributes ~30 of 50 and the smallest ~5, rather than a uniform draw
    /// that could miss a bank entirely.
    pub per_bank: BTreeMap<String, usize>,
    pub mismatches: Vec<Mismatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub dir: String,
    /// sha256 of the snapshot's own `SHA256SUMS`, which pins every other file
    /// — the same value `import` writes into `disposition.mg_import.snapshot`.
    pub sha256: String,
    pub banks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub snapshot: SnapshotInfo,
    /// Empty means the frozen `/stats` and the frozen archive agree with each
    /// other. A non-empty list is a *snapshot* failure, not a migration one.
    pub integrity: Vec<String>,
    pub tier1: Vec<Tier1Check>,
    pub tier2: Vec<Tier2Metric>,
    pub tier3: BTreeMap<String, serde_json::Value>,
    pub sample: SampleReport,
    pub verdict: Verdict,
    /// The sentence Phase F pastes into the cutover note. Generated here
    /// rather than written by hand later, which is the difference between
    /// evidence and recollection.
    pub sentence: String,
    /// Set when `--accept-tier2 <hash>` matched: a human acknowledged this
    /// exact Tier-2 result and the verdict was downgraded from REVIEW to PASS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier2_accepted: Option<String>,
}

impl Report {
    /// The hash `--accept-tier2` takes: sha256 over **the snapshot and the
    /// Tier-2 counts**, and nothing else.
    ///
    /// Deliberately not "the whole report minus the verdict", which is what
    /// the first version did. Two reasons, and the second is the one that
    /// forced the change:
    ///
    /// * an acknowledgement is *of a specific Tier-2 result on a specific
    ///   snapshot*. Folding Tier 1 in would invalidate it whenever an
    ///   unrelated count moved — and folding the verdict in would let the act
    ///   of accepting change the value that identifies what was accepted;
    /// * **it must not depend on floating-point formatting.** The whole-report
    ///   version hashed the out-degree `mean`, and one f64 —
    ///   `19.558139534883722` — came back as `19.55813953488372` after a JSON
    ///   round trip, so the hash an operator read out of a saved report did
    ///   not match the hash the next run computed. A hash nobody can paste is
    ///   not a re-entry criterion. Every field below is an integer or a
    ///   string; the ratios are `ours / legacy` and add nothing.
    pub fn acceptance_hash(&self) -> String {
        let mut material = format!("snapshot={}\n", self.snapshot.sha256);
        for metric in &self.tier2 {
            material.push_str(&format!(
                "{}|legacy={}|ours={}|ok={}\n",
                metric.name,
                metric
                    .legacy
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                metric.ours,
                metric.ok,
            ));
        }
        sha256_hex(material.as_bytes())
    }

    /// The human table. Three tiers, in order, with the failures legible
    /// without reading the JSON.
    pub fn table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "snapshot {} ({} banks)\n",
            self.snapshot.sha256, self.snapshot.banks
        ));
        if self.integrity.is_empty() {
            out.push_str("integrity  ok   the frozen /stats and the frozen archive agree\n");
        } else {
            for line in &self.integrity {
                out.push_str(&format!("integrity  FAIL {line}\n"));
            }
        }
        out.push_str("\nTIER 1 — equality (any mismatch fails the run)\n");
        for check in &self.tier1 {
            out.push_str(&format!(
                "  {} {:<30} expected {:>7}  actual {:>7}  [{}]{}\n",
                if check.ok { "ok  " } else { "FAIL" },
                check.name,
                check.expected,
                check.actual,
                check.source,
                check
                    .detail
                    .as_ref()
                    .map(|d| format!("\n       {d}"))
                    .unwrap_or_default(),
            ));
        }
        out.push_str(
            "\nTIER 2 — recomputed (reported, banded only where a band means something)\n",
        );
        for metric in &self.tier2 {
            let legacy = metric
                .legacy
                .map(|l| l.to_string())
                .unwrap_or_else(|| "—".to_string());
            let ratio = metric
                .ratio
                .map(|r| format!("{r:.3}x"))
                .unwrap_or_else(|| "—".to_string());
            let band = metric
                .band
                .map(|(lo, hi)| format!("band [{lo}, {hi}]"))
                .unwrap_or_else(|| "no band".to_string());
            out.push_str(&format!(
                "  {} {:<26} legacy {:>7}  ours {:>7}  {:>8}  {}\n       {}\n       out-degree: mean {:.2}, p50 {}, p90 {}, max {} over {} nodes\n",
                if metric.ok { "ok  " } else { "REVIEW" },
                metric.name,
                legacy,
                metric.ours,
                ratio,
                band,
                metric.note,
                metric.out_degree.mean,
                metric.out_degree.p50,
                metric.out_degree.p90,
                metric.out_degree.max,
                metric.out_degree.nodes,
            ));
        }
        out.push_str("\nTIER 3 — not applicable\n");
        for (name, value) in &self.tier3 {
            out.push_str(&format!("  n/a  {name}: {value}\n"));
        }
        out.push_str(&format!(
            "\nSAMPLE — {} of {} requested, seed {}, stratified {:?}\n",
            self.sample.drawn, self.sample.n, self.sample.seed, self.sample.per_bank
        ));
        for mismatch in &self.sample.mismatches {
            out.push_str(&format!(
                "  DIFF {} {} {}\n       legacy: {}\n       ours:   {}\n",
                mismatch.bank, mismatch.key, mismatch.field, mismatch.legacy, mismatch.ours
            ));
        }
        if self.sample.mismatches.is_empty() {
            out.push_str("  no content differences\n");
        }
        out.push_str(&format!(
            "\nVERDICT {:?} (exit {})\n{}\n",
            self.verdict,
            self.verdict.exit_code(),
            self.sentence
        ));
        out
    }
}

// ---------------------------------------------------------------------------
// options and the run
// ---------------------------------------------------------------------------

pub struct Options<'a> {
    pub snapshot: &'a Path,
    pub db: &'a Path,
    pub sample: usize,
    pub seed: u64,
    /// `--accept-tier2 <sha256>`: an explicit human acknowledgement of one
    /// specific Tier-2 result, which downgrades REVIEW to PASS **for that
    /// report only**.
    ///
    /// It exists because a phase that always exits 2 trains the reader to
    /// ignore exit 1 within two runs — the re-entry criterion for the
    /// exit-code split itself.
    pub accept_tier2: Option<&'a str>,
    /// Skip every comparison and emit what the database holds today.
    ///
    /// This is the runbook's step 3a: the cutover import's `--replace` deletes
    /// `sessions`, which is AC-2/AC-6 measurement data, and this is the only
    /// thing that preserves it. It reads a database that has not been migrated
    /// yet, so nothing here may require the snapshot to reconcile against it.
    pub dump_only: bool,
}

/// Reads the snapshot and the database, compares, and reports. **Writes
/// nothing** — every query goes through `Db::read`.
///
/// The honest form of "read-only" is narrower than the plan's first draft
/// claimed, and this is it: there is no read-only path in the store, because
/// `Db::open` runs `migrate::migrate` (`lib.rs:52-58`), eight `BEGIN
/// IMMEDIATE`s. What is true and testable is that `verify` issues no `INSERT`,
/// `UPDATE` or `DELETE`, and that it is safe against a live database because
/// migrations are already applied and each re-checks `user_version` inside its
/// own transaction before doing anything (`migrate.rs:44-48`).
pub fn run(opts: &Options<'_>) -> Result<Report> {
    snapshot::verify_sha256sums(opts.snapshot)?;
    let sums = std::fs::read(opts.snapshot.join("SHA256SUMS"))
        .map_err(|e| MigrateError::io(opts.snapshot.join("SHA256SUMS"), e))?;
    let snapshot_id = sha256_hex(&sums);

    let archives = super::archive::load_dir(opts.snapshot)?;
    let oracle = load_stats(opts.snapshot)?;
    let db = Db::open(opts.db).map_err(store)?;

    // Banks that were actually migrated: an archive with content. An empty
    // archive is skipped by `import` and must not be expected here either.
    let migrated: Vec<&BankArchive> = archives
        .iter()
        .filter(|a| !a.documents.is_empty() || !a.observations.is_empty())
        .collect();

    let info = SnapshotInfo {
        dir: opts.snapshot.display().to_string(),
        sha256: snapshot_id.clone(),
        banks: migrated.len(),
        exported_at: migrated
            .first()
            .and_then(|a| a.manifest.exported_at.clone()),
    };

    if opts.dump_only {
        return dump(info, &db);
    }

    let mut integrity = Vec::new();
    for archive in &migrated {
        let Some(stats) = oracle.get(&archive.bank_id) else {
            integrity.push(format!("{}: no /stats recorded", archive.bank_id));
            continue;
        };
        if let Err(e) = snapshot::assert_integrity(archive, stats) {
            integrity.push(e.to_string());
        }
    }
    // The dropped banks, re-checked from the frozen zeroes. "Nothing to lose"
    // is only true while it stays true, and two of the four are live
    // directories.
    for bank in snapshot::DROPPED_BANKS {
        if let Some(stats) = oracle.get(bank)
            && (stats.stats.total_nodes != 0 || stats.stats.total_documents != 0)
        {
            integrity.push(format!(
                "{bank} was dropped as empty but the snapshot records {} nodes / {} documents",
                stats.stats.total_nodes, stats.stats.total_documents
            ));
        }
    }

    let tier1 = tier1(&db, &migrated, &oracle, &snapshot_id)?;
    let tier2 = tier2(&db, &migrated, &oracle)?;
    let sample = sample(&db, &migrated, opts.sample, opts.seed)?;

    let tier1_ok = tier1.iter().all(|c| c.ok);
    let tier2_ok = tier2.iter().all(|m| m.ok);
    let clean = integrity.is_empty() && sample.mismatches.is_empty();

    let mut report = Report {
        snapshot: info,
        integrity,
        tier1,
        tier2,
        tier3: tier3(&oracle),
        sample,
        verdict: if !(tier1_ok && clean) {
            Verdict::Fail
        } else if tier2_ok {
            Verdict::Pass
        } else {
            Verdict::Review
        },
        sentence: String::new(),
        tier2_accepted: None,
    };
    report.sentence = sentence(&report);

    // The acknowledgement is checked *after* the verdict is computed and
    // against a hash that excludes the verdict, so accepting a Tier-2 result
    // can never mask a Tier-1 failure.
    if report.verdict == Verdict::Review
        && let Some(offered) = opts.accept_tier2
    {
        let hash = report.acceptance_hash();
        if offered == hash {
            report.verdict = Verdict::Pass;
            report.tier2_accepted = Some(hash);
            report.sentence = sentence(&report);
        }
    }
    Ok(report)
}

/// `--dump-only`: what the database holds, with no snapshot comparison.
fn dump(info: SnapshotInfo, db: &Db) -> Result<Report> {
    let mut tier1 = Vec::new();
    for (table, sql) in [
        ("banks", "SELECT count(*) FROM banks"),
        ("documents", "SELECT count(*) FROM documents"),
        ("memory_nodes", "SELECT count(*) FROM memory_nodes"),
        ("links", "SELECT count(*) FROM links"),
        ("entities", "SELECT count(*) FROM entities"),
        ("node_sources", "SELECT count(*) FROM node_sources"),
        ("sessions", "SELECT count(*) FROM sessions"),
        ("retain_jobs", "SELECT count(*) FROM retain_jobs"),
        ("benefit_ledger", "SELECT count(*) FROM benefit_ledger"),
        ("metric_snapshots", "SELECT count(*) FROM metric_snapshots"),
        (
            "consolidation_runs",
            "SELECT count(*) FROM consolidation_runs",
        ),
    ] {
        let n = read_i64(db, sql, [])?;
        tier1.push(Tier1Check {
            name: table.to_string(),
            expected: n,
            actual: n,
            source: "the database, as it stands".to_string(),
            ok: true,
            detail: None,
        });
    }
    Ok(Report {
        snapshot: info,
        integrity: Vec::new(),
        tier1,
        tier2: Vec::new(),
        tier3: BTreeMap::new(),
        sample: SampleReport {
            n: 0,
            seed: 0,
            drawn: 0,
            per_bank: BTreeMap::new(),
            mismatches: Vec::new(),
        },
        verdict: Verdict::Pass,
        sentence: "Dump only: no comparison was performed. This is the runbook's step 3a, \
                   which preserves the shadow run's sessions and retain_jobs before \
                   `import --replace` deletes them."
            .to_string(),
        tier2_accepted: None,
    })
}

// ---------------------------------------------------------------------------
// tier 1
// ---------------------------------------------------------------------------

fn tier1(
    db: &Db,
    migrated: &[&BankArchive],
    oracle: &BTreeMap<String, Stats>,
    snapshot_id: &str,
) -> Result<Vec<Tier1Check>> {
    let mut checks = Vec::new();
    let banks: Vec<&str> = migrated.iter().map(|a| a.bank_id.as_str()).collect();
    let scope = format!(
        "WHERE bank_id IN ({})",
        banks
            .iter()
            .map(|b| format!("'{}'", b.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",")
    );

    // --- what legacy counted -------------------------------------------------
    let stats =
        |f: fn(&Stats) -> i64| -> i64 { banks.iter().filter_map(|b| oracle.get(*b)).map(f).sum() };

    checks.push(Tier1Check::new(
        "banks",
        "the snapshot",
        migrated.len() as i64,
        read_i64(db, &format!("SELECT count(*) FROM banks {scope}"), [])?,
    ));
    checks.push(Tier1Check::new(
        "documents",
        "legacy /stats",
        stats(|s| s.stats.total_documents),
        read_i64(db, &format!("SELECT count(*) FROM documents {scope}"), [])?,
    ));
    checks.push(Tier1Check::new(
        "nodes",
        "legacy /stats",
        stats(|s| s.stats.total_nodes),
        read_i64(
            db,
            &format!("SELECT count(*) FROM memory_nodes {scope}"),
            [],
        )?,
    ));
    for fact_type in ["world", "experience", "observation"] {
        let legacy: i64 = banks
            .iter()
            .filter_map(|b| oracle.get(*b))
            .filter_map(|s| s.stats.nodes_by_fact_type.get(fact_type))
            .sum();
        checks.push(Tier1Check::new(
            format!("nodes.{fact_type}"),
            "legacy /stats",
            legacy,
            read_i64(
                db,
                &format!("SELECT count(*) FROM memory_nodes {scope} AND fact_type = '{fact_type}'"),
                [],
            )?,
        ));
    }
    checks.push(Tier1Check::new(
        "caused_by",
        "legacy /stats",
        stats(|s| s.stats.caused_by()),
        read_i64(
            db,
            &format!(
                "SELECT count(*) FROM links l JOIN memory_nodes n ON n.id = l.from_node_id
                 WHERE l.link_type = 'caused_by' AND n.bank_id IN ({})",
                quoted(&banks)
            ),
            [],
        )?,
    ));

    // --- what the archive said ----------------------------------------------
    let mut source_pairs = 0i64;
    let mut raw_sources = 0i64;
    let mut tags = 0i64;
    let mut entity_names: BTreeSet<(String, String)> = BTreeSet::new();
    let mut mentions = 0i64;
    for archive in migrated {
        for observation in &archive.observations {
            let distinct: BTreeSet<(&str, i64)> = observation
                .sources
                .iter()
                .map(|s| (s.document_id.as_str(), s.fact_index))
                .collect();
            source_pairs += distinct.len() as i64;
            raw_sources += observation.sources.len() as i64;
            tags += distinct_tags(&observation.tags);
        }
        for fact in archive.documents.iter().flat_map(|d| d.facts.iter()) {
            tags += distinct_tags(&fact.tags);
            let names = crate::entities::normalized_mentions(&fact.entities);
            mentions += names.len() as i64;
            entity_names.extend(names.into_iter().map(|n| (archive.bank_id.clone(), n)));
        }
    }
    checks.push(
        Tier1Check::new(
            "node_sources",
            "the archive, distinct pairs",
            source_pairs,
            read_i64(
                db,
                &format!(
                    "SELECT count(*) FROM node_sources s JOIN memory_nodes n ON n.id = s.observation_id
                     WHERE n.bank_id IN ({})",
                    quoted(&banks)
                ),
                [],
            )?,
        )
        .with_detail(format!(
            "{raw_sources} raw references collapse to {source_pairs} distinct — \
             link_sources_tx is INSERT OR IGNORE against the (observation_id, source_id) PK \
             (consolidate.rs:638-650)"
        )),
    );
    checks.push(Tier1Check::new(
        "node_tags",
        "the archive",
        tags,
        read_i64(
            db,
            &format!(
                "SELECT count(*) FROM node_tags t JOIN memory_nodes n ON n.id = t.node_id
                 WHERE n.bank_id IN ({})",
                quoted(&banks)
            ),
            [],
        )?,
    ));
    checks.push(
        Tier1Check::new(
            "entities",
            "the archive, normalized",
            entity_names.len() as i64,
            read_i64(db, &format!("SELECT count(*) FROM entities {scope}"), [])?,
        )
        .with_detail(
            "exact because MG-1b normalizes and stops — the fuzzy pass it removed dissolved \
             77 of 3,917 names (migrate/import.rs::write_entities)"
                .to_string(),
        ),
    );
    checks.push(Tier1Check::new(
        "node_entities",
        "the archive, normalized",
        mentions,
        read_i64(
            db,
            &format!(
                "SELECT count(*) FROM node_entities ne JOIN memory_nodes n ON n.id = ne.node_id
                 WHERE n.bank_id IN ({})",
                quoted(&banks)
            ),
            [],
        )?,
    ));

    // --- post-conditions only we can state ----------------------------------
    let marked = read_i64(
        db,
        &format!(
            "SELECT count(*) FROM banks {scope}
             AND json_extract(disposition, '$.{}.state') = 'done'
             AND json_extract(disposition, '$.{}.snapshot') = '{snapshot_id}'",
            super::import::MARKER_KEY,
            super::import::MARKER_KEY
        ),
        [],
    )?;
    checks.push(
        Tier1Check::new(
            "import marker",
            "our own rule",
            migrated.len() as i64,
            marked,
        )
        .with_detail(
            "state = done AND snapshot = this snapshot's hash — a bank imported from a \
             different snapshot is caught rather than counted"
                .to_string(),
        ),
    );
    let watermarks = read_i64(
        db,
        &format!(
            "SELECT count(*) FROM consolidation_runs r
             WHERE r.bank_id IN ({}) AND r.status = 'done'
               AND r.watermark = (SELECT MAX(id) FROM memory_nodes n WHERE n.bank_id = r.bank_id)",
            quoted(&banks)
        ),
        [],
    )?;
    checks.push(
        Tier1Check::new(
            "consolidation watermark",
            "our own rule",
            migrated.len() as i64,
            watermarks,
        )
        .with_detail(
            "one done run per bank at watermark = MAX(memory_nodes.id); without it the daemon \
             re-consolidates the whole migrated corpus within one poll of restart \
             (consolidate.rs:314-330)"
                .to_string(),
        ),
    );
    checks.push(
        Tier1Check::new(
            "embedding coverage",
            "our own rule",
            0,
            read_i64(
                db,
                &format!(
                    "SELECT count(*) FROM memory_nodes {scope}
                     AND (embedding IS NULL OR embedding_model <> ?1)"
                ),
                [memgarden_core::EMBEDDING_MODEL_ID],
            )?,
        )
        .with_detail(
            "a NULL or foreign producer drops a node out of the dense arm silently \
             (search.rs:317, 0005_embedding_model.sql:9-12). Non-zero after \
             --defer-embeddings until the daemon has drained the backlog"
                .to_string(),
        ),
    );
    checks.push(Tier1Check::new(
        "orphan facts",
        "our own rule",
        0,
        read_i64(
            db,
            &format!("SELECT count(*) FROM memory_nodes {scope} AND document_id IS NULL AND fact_type <> 'observation'"),
            [],
        )?,
    ));
    checks.push(
        Tier1Check::new(
            "semantic edges from observations",
            "our own rule",
            0,
            read_i64(
                db,
                &format!(
                    "SELECT count(*) FROM links l JOIN memory_nodes n ON n.id = l.from_node_id
                     WHERE l.link_type = 'semantic' AND n.fact_type = 'observation'
                       AND n.bank_id IN ({})",
                    quoted(&banks)
                ),
                [],
            )?,
        )
        .with_detail(
            "parity, not a divergence: legacy stores none either — /graph decomposed by \
             endpoint fact_type gives 4,603 == /stats 4,603 for bank-a"
                .to_string(),
        ),
    );

    // --- the one that catches a broken import ------------------------------
    let (stored, reference, extra, missing, covered) = temporal_self_consistency(db, &banks)?;
    checks.push(
        Tier1Check::new(
            "temporal self-consistency",
            "our own rule, re-run",
            reference,
            stored,
        )
        .with_detail(format!(
            "links.rs:62-92 replayed over the migrated nodes' own (fact_type, event_date): \
             {extra} stored edges the rule would not emit, {missing} the rule emits that are \
             not stored, over {covered} of {} banks. This replaces the equality against legacy \
             that can never hold",
            banks.len()
        )),
    );

    Ok(checks)
}

/// The stored `temporal` edge set against a reference run of `links.rs:62-92`
/// over the **migrated nodes' own** `(fact_type, event_date)`.
///
/// Exact and reproducible, and it is what catches a broken import, a lost
/// `event_date` or a mis-ordered batch — everything a Tier-1 temporal gate
/// against legacy was wanted for, without asserting a correspondence that does
/// not exist.
///
/// Reads the dates from the **database**, not from the archive. That is the
/// whole point: MG-1b stamps observation dates that
/// `consolidate::insert_observation` does not write, and if it stopped doing
/// so the import's own temporal pass would still emit the edges (it reads the
/// archive) while this check would go to zero on the observation side.
///
/// **Scoped to the nodes the import itself wrote, and that is not a
/// convenience.** The whole-corpus rule is a fixed point only of what `import`
/// produced; the daemon builds the same graph *incrementally*, calling
/// `links::temporal_links(&chunk, &window)` per retain (`retain/mod.rs:626`),
/// so a bank that has been retained into since is not a fixed point of the
/// whole-corpus rule and never will be. Unscoped, this reported 2,281 stored
/// against 2,460 on the live database — a failure that is the daemon working
/// correctly.
///
/// The scope is `metadata.$.legacy`, the key MG-1b stamps on every node it
/// writes and nothing else ever writes. Two earlier attempts were worse and
/// are worth naming: `id <= MAX(consolidation_runs.watermark)` looks like the
/// import's boundary and is not — **the daemon writes `consolidation_runs`
/// rows too**, so on the live database the scope drifted onto banks that were
/// never imported and the check failed on 1 of 4 of them. Scoping by the
/// import marker alone would have needed a boundary the marker does not carry.
///
/// It is exact rather than approximate: every edge a later retain adds has a
/// **new** node as its `from` (retain passes the new chunk as `new_nodes`), so
/// edges with both endpoints inside the migrated set are precisely the
/// import's.
///
/// `covered` is returned so a vacuous pass is visible: a database with no
/// imported bank has nothing to check, and the report says so rather than
/// printing a green row over an empty set.
fn temporal_self_consistency(db: &Db, banks: &[&str]) -> Result<(i64, i64, i64, i64, i64)> {
    let mut stored_total = 0i64;
    let mut reference_total = 0i64;
    let (mut extra, mut missing, mut covered) = (0i64, 0i64, 0i64);
    for bank in banks {
        let migrated_nodes = read_i64(
            db,
            "SELECT count(*) FROM memory_nodes
             WHERE bank_id = ?1 AND json_extract(metadata, '$.legacy') IS NOT NULL",
            [bank],
        )?;
        if migrated_nodes == 0 {
            continue;
        }
        covered += 1;
        let conn = db.read().map_err(store)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, fact_type, event_date FROM memory_nodes
                 WHERE bank_id = ?1 AND event_date IS NOT NULL
                   AND json_extract(metadata, '$.legacy') IS NOT NULL",
            )
            .map_err(sql)?;
        let timed: Vec<TimedNode> = stmt
            .query_map([bank], |r| {
                Ok(TimedNode {
                    id: r.get(0)?,
                    fact_type: r.get(1)?,
                    event_date: r.get(2)?,
                })
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        let expected: BTreeSet<(i64, i64)> = links::temporal_links(&timed, &timed)
            .into_iter()
            .map(|l| (l.from_node_id, l.to_node_id))
            .collect();

        let mut stmt = conn
            .prepare(
                "SELECT l.from_node_id, l.to_node_id FROM links l
                 JOIN memory_nodes f ON f.id = l.from_node_id
                 JOIN memory_nodes t ON t.id = l.to_node_id
                 WHERE l.link_type = 'temporal' AND f.bank_id = ?1
                   AND json_extract(f.metadata, '$.legacy') IS NOT NULL
                   AND json_extract(t.metadata, '$.legacy') IS NOT NULL",
            )
            .map_err(sql)?;
        let actual: BTreeSet<(i64, i64)> = stmt
            .query_map([bank], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .map_err(sql)?;

        stored_total += actual.len() as i64;
        reference_total += expected.len() as i64;
        extra += actual.difference(&expected).count() as i64;
        missing += expected.difference(&actual).count() as i64;
    }
    Ok((stored_total, reference_total, extra, missing, covered))
}

// ---------------------------------------------------------------------------
// tier 2 and tier 3
// ---------------------------------------------------------------------------

fn tier2(
    db: &Db,
    migrated: &[&BankArchive],
    oracle: &BTreeMap<String, Stats>,
) -> Result<Vec<Tier2Metric>> {
    let banks: Vec<&str> = migrated.iter().map(|a| a.bank_id.as_str()).collect();
    let legacy = |link_type: &str| -> i64 {
        banks
            .iter()
            .filter_map(|b| oracle.get(*b))
            .filter_map(|s| s.stats.links_by_link_type.get(link_type))
            .sum()
    };

    let fact_temporal = link_count(db, &banks, "temporal", "n.fact_type <> 'observation'")?;
    let obs_temporal = link_count(db, &banks, "temporal", "n.fact_type = 'observation'")?;
    let semantic = link_count(db, &banks, "semantic", "1=1")?;
    let legacy_temporal = legacy("temporal");
    let ratio = (legacy_temporal > 0).then(|| fact_temporal as f64 / legacy_temporal as f64);

    let mut metrics = vec![
        Tier2Metric {
            name: "temporal, fact to fact".to_string(),
            legacy: Some(legacy_temporal),
            ours: fact_temporal,
            ratio,
            band: Some(TEMPORAL_BAND),
            ok: ratio.is_none_or(|r| r >= TEMPORAL_BAND.0 && r <= TEMPORAL_BAND.1),
            note: "legacy's /stats temporal count is fact-to-fact: it stores no derived edge on \
                   an observation (relink filters fact_type IN ('experience','world'), \
                   pg/graph.py:606). Its neighbour query applies no 24h predicate where \
                   links.rs:69 does, so the sets differ by rule and not by ordering"
                .to_string(),
            out_degree: out_degree(db, &banks, "temporal", "n.fact_type <> 'observation'")?,
        },
        Tier2Metric {
            name: "temporal, observation to observation".to_string(),
            legacy: None,
            ours: obs_temporal,
            ratio: None,
            band: None,
            ok: true,
            note: "a new edge class: legacy has no counterpart, so there is no ratio to take. \
                   Folding these into the fact-to-fact ratio would pass a run in which the \
                   fact-edge rule silently broke"
                .to_string(),
            out_degree: out_degree(db, &banks, "temporal", "n.fact_type = 'observation'")?,
        },
        Tier2Metric {
            name: "semantic".to_string(),
            legacy: Some(legacy("semantic")),
            ours: semantic,
            ratio: (legacy("semantic") > 0).then(|| semantic as f64 / legacy("semantic") as f64),
            band: None,
            ok: true,
            note: "NO BAND, deliberately. Every semantic edge here connects two nodes embedded \
                   in the same batch of 8: embed_task.rs:178-179 builds node_types from the \
                   just-embedded batch and links.rs:143 drops every neighbour outside it. Over \
                   the same vectors a whole-corpus pass emits ~10x more. Banding this would be \
                   banding a CE-7 defect"
                .to_string(),
            out_degree: out_degree(db, &banks, "semantic", "1=1")?,
        },
    ];

    // `proof_count` cannot be equal by construction: legacy stores it with a
    // `or len(source_ids)` fallback (`export.py:457`) where we always derive it
    // from `node_sources` (`recount_proof_tx`, `consolidate.rs:658-666`).
    let mut differ = 0i64;
    for archive in migrated {
        for observation in &archive.observations {
            let distinct: BTreeSet<(&str, i64)> = observation
                .sources
                .iter()
                .map(|s| (s.document_id.as_str(), s.fact_index))
                .collect();
            if observation.proof_count != distinct.len() as i64 {
                differ += 1;
            }
        }
    }
    let observations: i64 = migrated.iter().map(|a| a.observations.len() as i64).sum();
    metrics.push(Tier2Metric {
        name: "proof_count disagreements".to_string(),
        legacy: Some(observations),
        ours: differ,
        ratio: None,
        band: None,
        ok: true,
        note: "reported, never gated: legacy's stored value falls back to len(source_ids) \
               (export.py:457) and ours is derived from what survived the INSERT OR IGNORE. \
               They are different schemas, not a resolution bug"
            .to_string(),
        out_degree: Degrees::default(),
    });
    Ok(metrics)
}

fn tier3(oracle: &BTreeMap<String, Stats>) -> BTreeMap<String, serde_json::Value> {
    let reported: i64 = oracle
        .values()
        .filter(|s| !s.dropped)
        .filter_map(|s| s.stats.links_by_link_type.get("entity"))
        .sum();
    let mut out = BTreeMap::new();
    out.insert(
        "entity links".to_string(),
        serde_json::json!({
            "legacy_reported": reported,
            "legacy_stored": 0,
            "ours": 0,
            "citation": ENTITY_CITATION,
        }),
    );
    out
}

// ---------------------------------------------------------------------------
// the 50-sample content diff
// ---------------------------------------------------------------------------

/// `splitmix64` — a deterministic PRNG in five lines rather than a dependency.
///
/// The sample has to reproduce from `(seed, snapshot)` alone, which is all
/// this needs to provide. `ponytail:` if a future check needs a real
/// distribution, that is when a crate earns its keep.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

enum Record<'a> {
    Fact(&'a str, i64, &'a TransferFact),
    Observation(&'a TransferObservation),
}

fn sample(db: &Db, migrated: &[&BankArchive], want: usize, seed: u64) -> Result<SampleReport> {
    let total: usize = migrated
        .iter()
        .map(|a| a.fact_count() + a.observations.len())
        .sum();
    let mut per_bank = BTreeMap::new();
    let mut mismatches = Vec::new();
    let mut drawn = 0usize;
    let mut rng = Rng(seed);

    for archive in migrated {
        let nodes = archive.fact_count() + archive.observations.len();
        // Stratified in proportion to node count, so the largest bank
        // contributes ~30 of 50 and the smallest ~5. A uniform draw over the
        // pooled corpus could miss a bank entirely.
        let quota = if total == 0 {
            0
        } else {
            ((want * nodes) as f64 / total as f64).round() as usize
        };
        if quota == 0 || nodes == 0 {
            continue;
        }
        // Deterministic order first, then a seeded draw over it — the archive's
        // own file order is not a promise.
        let mut records: Vec<Record> = Vec::with_capacity(nodes);
        for document in &archive.documents {
            for (index, fact) in document.facts.iter().enumerate() {
                records.push(Record::Fact(&document.id, index as i64, fact));
            }
        }
        for observation in &archive.observations {
            records.push(Record::Observation(observation));
        }
        records.sort_by_key(|r| match r {
            Record::Fact(document, index, _) => (0, (*document).to_string(), *index, String::new()),
            Record::Observation(o) => (
                1,
                String::new(),
                0,
                format!("{}\u{0}{}", o.text, o.mentioned_at.as_deref().unwrap_or("")),
            ),
        });

        let mut picked: BTreeSet<usize> = BTreeSet::new();
        let mut guard = 0;
        while picked.len() < quota.min(records.len()) && guard < quota * 64 + 1024 {
            picked.insert((rng.next() % records.len() as u64) as usize);
            guard += 1;
        }
        per_bank.insert(archive.bank_id.clone(), picked.len());
        drawn += picked.len();
        for index in picked {
            compare(db, &archive.bank_id, &records[index], &mut mismatches)?;
        }
    }

    Ok(SampleReport {
        n: want,
        seed,
        drawn,
        per_bank,
        mismatches,
    })
}

fn compare(db: &Db, bank: &str, record: &Record<'_>, out: &mut Vec<Mismatch>) -> Result<()> {
    let (key, row) = match record {
        Record::Fact(document, index, _) => (
            format!("{document}#{index}"),
            node_by_legacy_key(db, bank, document, *index)?,
        ),
        Record::Observation(o) => (
            format!(
                "observation:{}",
                &o.text.chars().take(40).collect::<String>()
            ),
            observation_by_text(db, bank, &o.text, o.mentioned_at.as_deref())?,
        ),
    };
    let Some(row) = row else {
        out.push(Mismatch {
            bank: bank.to_string(),
            key,
            field: "node".to_string(),
            legacy: "present in the archive".to_string(),
            ours: "no matching node".to_string(),
        });
        return Ok(());
    };

    let mut diff = |field: &str, legacy: String, ours: String| {
        if legacy != ours {
            out.push(Mismatch {
                bank: bank.to_string(),
                key: key.clone(),
                field: field.to_string(),
                legacy,
                ours,
            });
        }
    };

    let (text, fact_type, context, event_date, occurred_start, occurred_end, mentioned_at, id) =
        row;
    let (
        want_text,
        want_type,
        want_context,
        want_start,
        want_end,
        want_mentioned,
        want_tags,
        want_entities,
    ) = match record {
        Record::Fact(_, _, f) => (
            f.text.clone(),
            f.fact_type.clone(),
            f.context.clone().filter(|c| !c.is_empty()),
            f.occurred_start.clone(),
            f.occurred_end.clone(),
            f.mentioned_at.clone(),
            f.tags.clone(),
            crate::entities::normalized_mentions(&f.entities),
        ),
        Record::Observation(o) => (
            o.text.clone(),
            "observation".to_string(),
            None,
            o.occurred_start.clone(),
            o.occurred_end.clone(),
            o.mentioned_at.clone(),
            o.tags.clone(),
            Vec::new(),
        ),
    };

    diff("text", want_text, text);
    diff("fact_type", want_type, fact_type);
    // `""` and NULL are the same absence: legacy emits empty strings and
    // `NewNode.context` filters them.
    diff(
        "context",
        want_context.unwrap_or_default(),
        context.unwrap_or_default(),
    );
    diff(
        "occurred_start",
        ms(&want_start),
        occurred_start.map(|v| v.to_string()).unwrap_or_default(),
    );
    diff(
        "occurred_end",
        ms(&want_end),
        occurred_end.map(|v| v.to_string()).unwrap_or_default(),
    );
    diff(
        "mentioned_at",
        ms(&want_mentioned),
        mentioned_at.map(|v| v.to_string()).unwrap_or_default(),
    );
    // `writes.py:80` parity, asserted rather than assumed.
    let derived = if want_start.is_some() {
        ms(&want_start)
    } else {
        ms(&want_mentioned)
    };
    diff(
        "event_date",
        derived,
        event_date.map(|v| v.to_string()).unwrap_or_default(),
    );

    let ours_tags: BTreeSet<String> =
        strings(db, "SELECT tag FROM node_tags WHERE node_id = ?1", id)?;
    let want_tag_set: BTreeSet<String> = want_tags.into_iter().collect();
    diff(
        "tags",
        format!("{want_tag_set:?}"),
        format!("{ours_tags:?}"),
    );

    if matches!(record, Record::Fact(..)) {
        let ours_entities: BTreeSet<String> = strings(
            db,
            "SELECT e.canonical_name FROM entities e
             JOIN node_entities ne ON ne.entity_id = e.id WHERE ne.node_id = ?1",
            id,
        )?;
        let want_entity_set: BTreeSet<String> = want_entities.into_iter().collect();
        diff(
            "entities",
            format!("{want_entity_set:?}"),
            format!("{ours_entities:?}"),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the sentence
// ---------------------------------------------------------------------------

/// The sentence Phase F pastes into the cutover note, with the placeholders
/// filled from this run.
fn sentence(report: &Report) -> String {
    if report.verdict == Verdict::Fail {
        let failed: Vec<&str> = report
            .tier1
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect();
        return format!(
            "AC-3 is NOT met from this run. Tier-1 mismatches: {}. Content differences: {}. \
             Snapshot integrity: {}.",
            if failed.is_empty() {
                "none".to_string()
            } else {
                failed.join(", ")
            },
            report.sample.mismatches.len(),
            if report.integrity.is_empty() {
                "ok"
            } else {
                "FAILED"
            },
        );
    }
    let find = |name: &str| report.tier2.iter().find(|m| m.name == name);
    let temporal = find("temporal, fact to fact");
    let semantic = find("semantic");
    let stored = report
        .tier1
        .iter()
        .find(|c| c.name == "temporal self-consistency")
        .map(|c| c.actual)
        .unwrap_or(0);
    format!(
        "No fact, observation, document or authored causal relation was lost. Derived adjacency \
         (semantic, temporal) was rebuilt from the migrated facts by MemGarden's own rules \
         rather than copied: a semantic edge is a function of the vector space and ours is not \
         legacy's, and legacy's temporal rule applies no 24-hour window to its neighbour query \
         (ops_postgresql.py:562-593) where ours does. The rebuilt temporal set reproduces our \
         own rule exactly ({stored} edges, checked against a reference implementation) and its \
         fact-to-fact half stands at {}x legacy's count; semantic stands at {}x, and that ratio \
         is a CE-7 defect rather than a migration property — every semantic edge here is \
         confined to one embedding batch. Observation-to-observation temporal edges ({}) are a \
         class legacy does not store. Entity links exist in neither system's storage.{}",
        temporal
            .and_then(|m| m.ratio)
            .map(|r| format!("{r:.2}"))
            .unwrap_or_else(|| "—".to_string()),
        semantic
            .and_then(|m| m.ratio)
            .map(|r| format!("{r:.2}"))
            .unwrap_or_else(|| "—".to_string()),
        find("temporal, observation to observation")
            .map(|m| m.ours)
            .unwrap_or(0),
        if report.tier2_accepted.is_some() {
            " A Tier-2 review stop was explicitly acknowledged for this report."
        } else {
            ""
        },
    )
}

// ---------------------------------------------------------------------------
// small reads
// ---------------------------------------------------------------------------

fn quoted(banks: &[&str]) -> String {
    banks
        .iter()
        .map(|b| format!("'{}'", b.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

fn distinct_tags(tags: &[String]) -> i64 {
    tags.iter().collect::<BTreeSet<&String>>().len() as i64
}

fn ms_i64(value: &Option<String>) -> Option<i64> {
    value
        .as_deref()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_millisecond())
}

fn ms(value: &Option<String>) -> String {
    ms_i64(value).map(|v| v.to_string()).unwrap_or_default()
}

type NodeRow = (
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
);

const NODE_COLUMNS: &str =
    "text, fact_type, context, event_date, occurred_start, occurred_end, mentioned_at, id";

fn node_row(row: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn node_by_legacy_key(db: &Db, bank: &str, document: &str, index: i64) -> Result<Option<NodeRow>> {
    use rusqlite::OptionalExtension;
    db.read()
        .map_err(store)?
        .query_row(
            &format!(
                "SELECT {NODE_COLUMNS} FROM memory_nodes
                 WHERE bank_id = ?1 AND json_extract(metadata, '$.legacy.document_id') = ?2
                   AND json_extract(metadata, '$.legacy.fact_index') = ?3"
            ),
            rusqlite::params![bank, document, index],
            node_row,
        )
        .optional()
        .map_err(sql)
}

/// Observations join on `(text, mentioned_at)` — measured 0 duplicates over
/// all 1,747, where `text` alone collides 3 times.
///
/// `mentioned_at` is bound as an **integer** and matched with `IS`, not with
/// `=` against a string. The first version bound the epoch as text and
/// compared it to `coalesce(mentioned_at, '')`: `coalesce` strips the column's
/// INTEGER affinity, so SQLite never converted the operand and *every*
/// observation in the sample came back "no matching node" — 18 false content
/// differences and an exit 1 on a database that was correct. A join key that
/// silently matches nothing is worse than no join key, because it reads as a
/// migration failure.
fn observation_by_text(
    db: &Db,
    bank: &str,
    text: &str,
    mentioned_at: Option<&str>,
) -> Result<Option<NodeRow>> {
    use rusqlite::OptionalExtension;
    let at = ms_i64(&mentioned_at.map(str::to_string));
    db.read()
        .map_err(store)?
        .query_row(
            &format!(
                "SELECT {NODE_COLUMNS} FROM memory_nodes
                 WHERE bank_id = ?1 AND fact_type = 'observation' AND text = ?2
                   AND mentioned_at IS ?3"
            ),
            rusqlite::params![bank, text, at],
            node_row,
        )
        .optional()
        .map_err(sql)
}

fn strings(db: &Db, sql_text: &str, id: i64) -> Result<BTreeSet<String>> {
    let conn = db.read().map_err(store)?;
    let mut stmt = conn.prepare(sql_text).map_err(sql)?;
    let rows = stmt.query_map([id], |r| r.get(0)).map_err(sql)?;
    rows.collect::<rusqlite::Result<BTreeSet<String>>>()
        .map_err(sql)
}

fn link_count(db: &Db, banks: &[&str], link_type: &str, side: &str) -> Result<i64> {
    read_i64(
        db,
        &format!(
            "SELECT count(*) FROM links l JOIN memory_nodes n ON n.id = l.from_node_id
             WHERE l.link_type = '{link_type}' AND {side} AND n.bank_id IN ({})",
            quoted(banks)
        ),
        [],
    )
}

fn out_degree(db: &Db, banks: &[&str], link_type: &str, side: &str) -> Result<Degrees> {
    let conn = db.read().map_err(store)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT count(*) FROM links l JOIN memory_nodes n ON n.id = l.from_node_id
             WHERE l.link_type = '{link_type}' AND {side} AND n.bank_id IN ({})
             GROUP BY l.from_node_id",
            quoted(banks)
        ))
        .map_err(sql)?;
    let counts = stmt
        .query_map([], |r| r.get(0))
        .map_err(sql)?
        .collect::<rusqlite::Result<Vec<i64>>>()
        .map_err(sql)?;
    Ok(Degrees::of(counts))
}

fn read_i64<P: rusqlite::Params>(db: &Db, sql_text: &str, params: P) -> Result<i64> {
    db.read()
        .map_err(store)?
        .query_row(sql_text, params, |r| r.get(0))
        .map_err(sql)
}

fn sql(e: rusqlite::Error) -> MigrateError {
    MigrateError::Store {
        message: e.to_string(),
    }
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

    /// The same deterministic stand-in `import`'s tests use — one basis vector
    /// per text. `verify` reads no vector, only the `embedding_model` stamp.
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

    /// A migrated database and the snapshot it came from — the only state
    /// `verify` ever sees.
    struct Migrated {
        snapshot: Snapshot,
        _dir: tempfile::TempDir,
        db: std::path::PathBuf,
    }

    impl Migrated {
        async fn of(snapshot: Snapshot) -> Migrated {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("m.db");
            let cfg = memgarden_core::config::Config::defaults().unwrap();
            crate::migrate::import::run(&crate::migrate::import::Options {
                snapshot: snapshot.path(),
                db: &db,
                replace: false,
                cfg: &cfg,
                embed: &stub,
                // The fact backlog needs the real model, so these databases
                // land with `embedding IS NULL` on every fact — which is
                // exactly the state `--defer-embeddings` leaves and is why the
                // embedding-coverage check is exercised by `relax`.
                drain: None,
            })
            .await
            .expect("the fixture imports");
            Migrated {
                snapshot,
                _dir: dir,
                db,
            }
        }

        async fn real() -> Migrated {
            Migrated::of(Snapshot::real()).await
        }

        /// `Snapshot::edit` reseals `SHA256SUMS`, so editing the oracle after
        /// an import changes the snapshot hash the marker recorded and Tier 1
        /// correctly fails on it — that check is doing its job, and it makes
        /// stats.json unusable as a lever *after* the fact. Edit first, import
        /// second.
        async fn with_legacy_temporal(count: i64) -> Migrated {
            let snapshot = Snapshot::real();
            snapshot.edit("stats.json", |stats| {
                stats[JCODE]["stats"]["links_by_link_type"]["temporal"] = json(count)
            });
            let m = Migrated::of(snapshot).await;
            m.as_if_drained();
            m
        }

        /// Our own fact-to-fact temporal count for this fixture, which the
        /// band tests need before they can choose a legacy number.
        async fn our_fact_temporal() -> i64 {
            let m = Migrated::real().await;
            m.as_if_drained().verify().tier2[0].ours
        }

        fn options(&self) -> Options<'_> {
            Options {
                snapshot: self.snapshot.path(),
                db: &self.db,
                sample: 50,
                seed: 1,
                accept_tier2: None,
                dump_only: false,
            }
        }

        fn verify(&self) -> Report {
            run(&self.options()).expect("verify runs")
        }

        /// Every fixture imports with `drain: None`, so the embedding-coverage
        /// check would fail for a reason that is about the *test harness* and
        /// not about anything under test. Stamping the producer is the
        /// smallest thing that removes it from the way.
        fn as_if_drained(&self) -> &Self {
            self.write(&format!(
                "UPDATE memory_nodes SET embedding_model = '{}'
                 WHERE embedding_model IS NULL",
                memgarden_core::EMBEDDING_MODEL_ID
            ));
            self.write(
                "UPDATE memory_nodes SET embedding = zeroblob(1536) WHERE embedding IS NULL",
            );
            self
        }

        fn write(&self, sql: &str) {
            let db = Db::open(&self.db).unwrap();
            db.write(|tx| {
                tx.execute_batch(sql)
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))
            })
            .unwrap();
        }

        fn open(&self) -> Db {
            Db::open(&self.db).unwrap()
        }
    }

    fn failed(report: &Report) -> Vec<&str> {
        report
            .tier1
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect()
    }

    // --- the happy path, and what it is allowed to say ----------------------

    #[tokio::test]
    async fn a_correctly_migrated_bank_passes_every_tier_and_the_sample() {
        let m = Migrated::real().await;
        let report = m.as_if_drained().verify();
        assert_eq!(failed(&report), Vec::<&str>::new());
        assert!(report.integrity.is_empty());
        assert!(report.sample.mismatches.is_empty(), "{:?}", report.sample);
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.verdict.exit_code(), 0);
        assert!(
            report
                .sentence
                .starts_with("No fact, observation, document")
        );
        // Tier 3 is stated, not computed: both systems store zero.
        assert!(
            report.tier3["entity links"]["citation"]
                .as_str()
                .unwrap()
                .contains("counts.py:47-49")
        );
    }

    /// The observation half of the sample, which is the half that silently did
    /// nothing in the first version: `mentioned_at` was bound as text against
    /// `coalesce(mentioned_at, '')`, `coalesce` stripped the column's INTEGER
    /// affinity, and **every** observation came back "no matching node" — 18
    /// false content differences on a correct database. A join key that
    /// matches nothing reads as a migration failure.
    #[tokio::test]
    async fn every_observation_in_the_sample_finds_its_node() {
        let m = Migrated::real().await;
        // Big enough that the stratified quota covers every record.
        let report = run(&Options {
            sample: 10_000,
            ..m.as_if_drained().options()
        })
        .unwrap();
        assert_eq!(report.sample.drawn, 165, "86 facts + 79 observations");
        assert!(report.sample.mismatches.is_empty(), "{:?}", report.sample);
    }

    // --- tier 1 -------------------------------------------------------------

    /// Each mutation is to the **database**, not to the snapshot: Tier 1 asks
    /// "did the migration lose something", and a snapshot that disagrees with
    /// itself is the separate `integrity` failure.
    #[tokio::test]
    async fn every_tier1_field_fails_by_name_and_the_verdict_is_fail() {
        for (sql, field) in [
            (
                "DELETE FROM memory_nodes WHERE fact_type = 'world' AND id = (SELECT MIN(id) FROM memory_nodes)",
                "nodes",
            ),
            ("DELETE FROM documents", "documents"),
            (
                "DELETE FROM node_tags WHERE node_id = (SELECT MIN(node_id) FROM node_tags)",
                "node_tags",
            ),
            (
                "DELETE FROM node_sources WHERE observation_id = (SELECT MIN(observation_id) FROM node_sources)",
                "node_sources",
            ),
            (
                "DELETE FROM entities WHERE id = (SELECT MIN(id) FROM entities)",
                "entities",
            ),
            (
                "DELETE FROM links WHERE link_type = 'caused_by'",
                "caused_by",
            ),
            (
                "DELETE FROM links WHERE link_type = 'temporal'",
                "temporal self-consistency",
            ),
            ("DELETE FROM consolidation_runs", "consolidation watermark"),
            (
                "UPDATE banks SET disposition = json_set(disposition, '$.mg_import.state', 'running')",
                "import marker",
            ),
            (
                "UPDATE memory_nodes SET embedding_model = 'sentence-transformers:BAAI/bge-small-en-v1.5' WHERE fact_type = 'observation'",
                "embedding coverage",
            ),
            (
                "UPDATE memory_nodes SET document_id = NULL WHERE fact_type <> 'observation'",
                "orphan facts",
            ),
        ] {
            let m = Migrated::real().await;
            m.as_if_drained().write(sql);
            let report = m.verify();
            assert!(
                failed(&report).contains(&field),
                "{field} did not fail after `{sql}`; failures were {:?}",
                failed(&report)
            );
            assert_eq!(report.verdict, Verdict::Fail);
            assert_eq!(report.verdict.exit_code(), 1);
            assert!(report.sentence.contains(field), "{}", report.sentence);
        }
    }

    /// The check that catches a broken import rather than a disagreement with
    /// legacy: an edge the rule would not emit is as much a failure as a
    /// missing one, and the detail says which direction.
    #[tokio::test]
    async fn an_invented_temporal_edge_fails_self_consistency_too() {
        let m = Migrated::real().await;
        m.as_if_drained().write(
            "INSERT INTO links (from_node_id, to_node_id, link_type, entity_id, weight, created_at)
             SELECT (SELECT MIN(id) FROM memory_nodes), (SELECT MAX(id) FROM memory_nodes),
                    'temporal', 0, 0.5, 1
             WHERE NOT EXISTS (SELECT 1 FROM links
                               WHERE from_node_id = (SELECT MIN(id) FROM memory_nodes)
                                 AND to_node_id = (SELECT MAX(id) FROM memory_nodes)
                                 AND link_type = 'temporal')",
        );
        let report = m.verify();
        let check = report
            .tier1
            .iter()
            .find(|c| c.name == "temporal self-consistency")
            .unwrap();
        assert!(!check.ok);
        assert!(
            check
                .detail
                .as_ref()
                .unwrap()
                .contains("1 stored edges the rule would not emit"),
            "{:?}",
            check.detail
        );
    }

    /// A bank the daemon has retained into since the import must still pass.
    ///
    /// The whole-corpus rule is a fixed point only of what `import` wrote —
    /// retain builds the same graph incrementally, one chunk against a rolling
    /// window — so an unscoped check fails on a *correctly working* daemon.
    /// Measured on the live database before the scope went in: 2,281 stored
    /// against 2,460 expected.
    #[tokio::test]
    async fn a_bank_retained_into_after_the_import_still_passes_self_consistency() {
        let m = Migrated::real().await;
        m.as_if_drained();
        let before = m.verify();
        assert!(
            before
                .tier1
                .iter()
                .find(|c| c.name == "temporal self-consistency")
                .unwrap()
                .ok
        );

        // What a retain does: a new node above the watermark, and an edge from
        // it into the imported set. Retain always passes the *new* chunk as
        // `new_nodes`, so `from` is always the new node.
        m.write(&format!(
            "INSERT INTO memory_nodes
               (uuid, bank_id, fact_type, text, event_date, created_at, updated_at,
                embedding, embedding_model)
             VALUES ('after-the-import', '{JCODE}', 'world', 'a fact the daemon retained',
                     (SELECT MIN(event_date) FROM memory_nodes WHERE event_date IS NOT NULL),
                     1, 1, zeroblob(1536), '{}');
             INSERT INTO links (from_node_id, to_node_id, link_type, entity_id, weight, created_at)
             SELECT (SELECT MAX(id) FROM memory_nodes),
                    (SELECT MIN(id) FROM memory_nodes), 'temporal', 0, 0.9, 1;",
            memgarden_core::EMBEDDING_MODEL_ID
        ));

        let after = m.verify();
        let check = after
            .tier1
            .iter()
            .find(|c| c.name == "temporal self-consistency")
            .unwrap();
        assert!(check.ok, "{:?}", check);
        assert_eq!(
            check.actual,
            before
                .tier1
                .iter()
                .find(|c| c.name == "temporal self-consistency")
                .unwrap()
                .actual,
            "the daemon's later edge is outside the scope, not counted as an extra"
        );
        assert!(check.detail.as_ref().unwrap().contains("over 1 of 1 banks"));
    }

    /// And the vacuous case is visible rather than green: a database with no
    /// imported bank has nothing for this check to say.
    #[tokio::test]
    async fn a_database_with_no_migrated_node_says_the_check_covered_nothing() {
        let m = Migrated::real().await;
        m.as_if_drained()
            .write("UPDATE memory_nodes SET metadata = NULL");
        let report = m.verify();
        let check = report
            .tier1
            .iter()
            .find(|c| c.name == "temporal self-consistency")
            .unwrap();
        assert!(check.detail.as_ref().unwrap().contains("over 0 of 1 banks"));
        assert_eq!(check.expected, 0);
        assert_eq!(check.actual, 0);
        // And the state is not silently fine: without the legacy key there is
        // no migration to speak of, and the sample says so.
        assert!(!report.sample.mismatches.is_empty());
    }

    // --- tier 2 -------------------------------------------------------------

    /// A Tier-2 ratio outside the band is a **review stop**, not a failure:
    /// exit 2, verdict REVIEW, and every Tier-1 check still green.
    #[tokio::test]
    async fn a_ratio_outside_the_band_exits_2_and_does_not_mask_tier_1() {
        // Our own fact-to-fact count is what it is; moving legacy's side is
        // what takes the ratio out of the band.
        let m = Migrated::with_legacy_temporal(Migrated::our_fact_temporal().await * 10).await;
        let report = m.verify();
        assert_eq!(report.verdict, Verdict::Review);
        assert_eq!(report.verdict.exit_code(), 2);
        assert!(!report.tier2[0].ok);
        assert_eq!(failed(&report), Vec::<&str>::new(), "tier 1 is untouched");

        // And a Tier-1 failure underneath still wins: a review stop must never
        // downgrade a failure.
        m.write("DELETE FROM documents");
        let report = m.verify();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(!report.tier2[0].ok, "the tier-2 breach is still reported");
    }

    /// In-band is a pass, and the band is on **fact-to-fact** edges only —
    /// folding the observation class in would pass a run in which the
    /// fact-edge rule silently broke.
    #[tokio::test]
    async fn the_band_is_over_fact_edges_and_the_observation_class_has_no_ratio() {
        // Legacy exactly at our fact-to-fact count / 1.6 — in band.
        let facts = Migrated::our_fact_temporal().await;
        let m = Migrated::with_legacy_temporal((facts as f64 / 1.6).round() as i64).await;
        let report = m.verify();
        assert_eq!(report.verdict, Verdict::Pass);
        let temporal = &report.tier2[0];
        assert!(temporal.ratio.unwrap() > 1.45 && temporal.ratio.unwrap() < 1.75);
        assert!(
            temporal.ours < report.tier2[1].ours + temporal.ours,
            "the two classes are separate"
        );
        assert_eq!(report.tier2[1].legacy, None, "legacy has no counterpart");
        assert_eq!(report.tier2[1].ratio, None);
        assert!(report.tier2[1].ok, "a metric with no band cannot fail");
        // Semantic is reported and never banded — see the module docs.
        let semantic = report.tier2.iter().find(|m| m.name == "semantic").unwrap();
        assert_eq!(semantic.band, None);
        assert!(semantic.note.contains("NO BAND"));
    }

    // --- the sample ---------------------------------------------------------

    #[tokio::test]
    async fn the_sample_is_deterministic_for_a_seed_and_stratified_by_bank() {
        let m = Migrated::of(Snapshot::both()).await;
        m.as_if_drained();
        let a = m.verify().sample;
        let b = m.verify().sample;
        assert_eq!(a.per_bank, b.per_bank);
        assert_eq!(a.drawn, b.drawn);

        // 165 jcode nodes and 135 cms nodes out of 300, so 50 splits ~28/~22 —
        // proportional, not uniform, so neither bank can be missed.
        assert_eq!(a.per_bank.len(), 2);
        let jcode = a.per_bank[JCODE];
        let cms = a.per_bank["claude-code::bank-b"];
        assert!(
            jcode > cms,
            "the larger bank contributes more: {jcode} vs {cms}"
        );
        assert!(cms >= 15, "and the smaller one is never zero: {cms}");

        let other = run(&Options {
            seed: 99,
            ..m.options()
        })
        .unwrap()
        .sample;
        assert_eq!(
            other.per_bank.values().sum::<usize>(),
            a.per_bank.values().sum::<usize>()
        );
    }

    /// A planted text mutation is caught and **both sides are printed**, which
    /// is what makes the diff actionable rather than a count.
    #[tokio::test]
    async fn a_planted_text_change_is_caught_with_both_sides() {
        let m = Migrated::real().await;
        m.as_if_drained()
            .write("UPDATE memory_nodes SET text = 'tampered' WHERE fact_type <> 'observation'");
        let report = run(&Options {
            sample: 10_000,
            ..m.options()
        })
        .unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        let diff = report
            .sample
            .mismatches
            .iter()
            .find(|d| d.field == "text")
            .expect("a text diff");
        assert_eq!(diff.ours, "tampered");
        assert_ne!(diff.legacy, "tampered");
        assert!(diff.key.contains('#'), "keyed on (document, fact_index)");
        assert!(
            report.table().contains("legacy:"),
            "both sides in the table"
        );
    }

    /// `""` and NULL are the same absence — legacy emits empty strings and
    /// `NewNode.context` filters them (`recall_bench.rs:209`). A diff here
    /// would fail every run on `edge::legal-but-absent`.
    #[tokio::test]
    async fn an_empty_context_is_not_a_content_difference() {
        let snapshot = Snapshot::edge("edge__legal-but-absent", "edge::legal-but-absent");
        snapshot.edit("edge__legal-but-absent/documents/000000.json", |doc| {
            doc["facts"][1]["causal_relations"][0]["target_fact_index"] = json(0)
        });
        let m = Migrated::of(snapshot).await;
        m.as_if_drained();
        let db = m.open();
        assert_eq!(
            db.read()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM memory_nodes
                     WHERE context IS NULL AND fact_type <> 'observation'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            3,
            "the archive's `context: \"\"` landed as NULL"
        );
        let report = run(&Options {
            sample: 10_000,
            ..m.options()
        })
        .unwrap();
        assert!(
            !report
                .sample
                .mismatches
                .iter()
                .any(|d| d.field == "context"),
            "{:?}",
            report.sample.mismatches
        );
    }

    // --- the report itself ---------------------------------------------------

    #[tokio::test]
    async fn the_report_round_trips_and_its_verdict_agrees_with_the_exit_code() {
        let m = Migrated::real().await;
        let report = m.as_if_drained().verify();
        let json = serde_json::to_string(&report).unwrap();
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, report.verdict);
        assert_eq!(back.tier1.len(), report.tier1.len());
        assert_eq!(back.sentence, report.sentence);
        assert_eq!(
            back.acceptance_hash(),
            report.acceptance_hash(),
            "the hash must survive a save-and-reload, or nobody can paste it"
        );
        assert_eq!(report.acceptance_hash().len(), 64);
        for (verdict, code) in [
            (Verdict::Pass, 0u8),
            (Verdict::Fail, 1),
            (Verdict::Review, 2),
        ] {
            assert_eq!(verdict.exit_code(), code);
        }
    }

    /// The escape hatch, and the reason it exists: a phase that always exits 2
    /// trains the reader to ignore exit 1 within two runs.
    #[tokio::test]
    async fn accept_tier2_downgrades_this_report_and_only_this_one() {
        let m = Migrated::with_legacy_temporal(Migrated::our_fact_temporal().await * 10).await;
        let review = m.verify();
        assert_eq!(review.verdict, Verdict::Review);
        let hash = review.acceptance_hash();

        let accepted = run(&Options {
            accept_tier2: Some(&hash),
            ..m.options()
        })
        .unwrap();
        assert_eq!(accepted.verdict, Verdict::Pass);
        assert_eq!(accepted.tier2_accepted.as_deref(), Some(hash.as_str()));
        assert!(accepted.sentence.contains("explicitly acknowledged"));

        let stale = run(&Options {
            accept_tier2: Some("0".repeat(64).as_str()),
            ..m.options()
        })
        .unwrap();
        assert_eq!(
            stale.verdict,
            Verdict::Review,
            "a stale hash accepts nothing"
        );

        // "and only this one" — the name's second half, which needs a *second*
        // Tier-2 result to mean anything. A different legacy count is a
        // different acknowledgement, and the first one's hash must not carry
        // over to it.
        let other = Migrated::with_legacy_temporal(Migrated::our_fact_temporal().await * 20).await;
        assert_ne!(other.verify().acceptance_hash(), hash);
        let not_accepted = run(&Options {
            accept_tier2: Some(&hash),
            ..other.options()
        })
        .unwrap();
        assert_eq!(not_accepted.verdict, Verdict::Review);

        // And it can never launder a Tier-1 failure: the hash is computed over
        // a report with the verdict removed, and the downgrade only applies to
        // REVIEW.
        m.write("DELETE FROM documents");
        let failing = m.verify();
        let hash = failing.acceptance_hash();
        let still_failing = run(&Options {
            accept_tier2: Some(&hash),
            ..m.options()
        })
        .unwrap();
        assert_eq!(still_failing.verdict, Verdict::Fail);
    }

    /// The honest form of "read-only". There is no read-only path in the store
    /// — `Db::open` runs eight migrations under `BEGIN IMMEDIATE`
    /// (`lib.rs:52-58`) — so the guarantee is narrower and this is it:
    /// `verify` issues no `INSERT`, `UPDATE` or `DELETE`.
    #[tokio::test]
    async fn verify_changes_nothing_in_the_database() {
        let m = Migrated::real().await;
        m.as_if_drained();
        let census = || -> Vec<(String, i64)> {
            let db = m.open();
            let conn = db.read().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table'
                       AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let tables: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            tables
                .into_iter()
                .filter_map(|t| {
                    conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))
                        .ok()
                        .map(|n: i64| (t, n))
                })
                .collect()
        };
        let before = census();
        assert!(before.len() > 10, "the census covers the schema");
        let report = m.verify();
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(before, census(), "verify wrote a row");
    }

    /// The runbook's step 3a: the only thing that preserves the shadow run's
    /// `sessions` before `import --replace` deletes them, and it must work on
    /// a database the snapshot has nothing to say about.
    #[tokio::test]
    async fn dump_only_reports_the_database_without_comparing_anything() {
        let m = Migrated::real().await;
        m.write("DELETE FROM documents");
        let report = run(&Options {
            dump_only: true,
            ..m.options()
        })
        .unwrap();
        assert_eq!(report.verdict, Verdict::Pass, "a dump cannot fail a gate");
        assert!(report.tier2.is_empty() && report.sample.mismatches.is_empty());
        let names: Vec<&str> = report.tier1.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"sessions") && names.contains(&"retain_jobs"));
        assert!(report.sentence.contains("step 3a"));
    }

    fn json(v: i64) -> serde_json::Value {
        serde_json::json!(v)
    }
}
