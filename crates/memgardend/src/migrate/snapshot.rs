//! `mg-migrate snapshot` — freeze legacy on disk, then refuse if it does not
//! reconcile.
//!
//! # Why a snapshot at all, rather than reading legacy at import time
//!
//! The legacy banks are **still being written**: both hook sets are live and
//! `claude-code::bank-b.last_document_at` moved during the
//! measurements this module's constants come from. AX-2 already paid for this
//! lesson once (`docs/design/ax-2-recall-quality.md:35-54`) — *"a re-fetch
//! returns a different corpus and would silently invalidate every label."*
//! Three consequences, and the third is the one that decides it:
//!
//! 1. `import` and `verify` must see the same bytes, or a fact written between
//!    them surfaces as a count mismatch that is not a migration defect;
//! 2. the run is reproducible from the artifact rather than from a daemon;
//! 3. **AC-3's evidence has to outlive the legacy daemon.** Phase F retires
//!    :9077, and a verification report whose oracle is a process that no
//!    longer exists is not evidence.
//!
//! # GETs only
//!
//! Cross-PR rule 1: *"Phase D adds: `mg-migrate` contains no code path that
//! issues anything but `GET` to :9077."* The honest form of that guarantee is
//! structural rather than a review promise — [`get`] is the only function in
//! this module that constructs a request, it takes a URL and returns bytes,
//! and every endpoint below goes through it. The checkable form of that:
//! `grep -nE '\.(post|put|patch|delete)\(' crates/memgardend/src/migrate/`
//! returns nothing, and `get` is the only `reqwest` call site in the module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::archive::{BankArchive, SUPPORTED_SCHEMA_VERSION, load_dir};
use super::{MigrateError, Result};

/// The legacy daemon. Loopback, plain HTTP — `reqwest` is declared in this
/// workspace with no TLS stack at all (`Cargo.toml`, B2), and there is nothing
/// on 127.0.0.1 to negotiate a certificate for.
pub const LEGACY_BASE: &str = "http://127.0.0.1:9077";

/// Banks the operator has decided not to migrate: nothing to lose (0 nodes, 0
/// documents, 0 links), and `hook session-start`'s `POST /v1/banks`
/// (`crates/memgarden-cli/src/cmd/session_start.rs:159-166`) recreates any of
/// them on first use.
///
/// **Named, not derived from emptiness**, and passed per run rather than
/// compiled in. Deriving the drop set from "is it empty right now" would make
/// the emptiness assertion circular and unable to fire: naming a bank is a
/// claim that it holds nothing, and [`assert_dropped_bank_empty`] re-checks
/// that claim on every run, because a dropped bank can be a live directory and
/// "nothing to lose" is only true while it stays true.
///
/// **Empty is the right default, and it is not a degraded mode.** A bank left
/// off this list is snapshotted whether or not it has content, and an empty
/// archive is then skipped at import — so an operator who names nothing loses
/// nothing. What they give up is only the assertion, which they had no basis
/// to make about someone else's banks anyway.
///
/// Whatever is passed is frozen into the snapshot as [`Stats::dropped`], which
/// is what `verify` reads. The decision therefore travels with the snapshot
/// rather than having to be re-supplied, and re-supplied identically, hours
/// later at verification time.
pub type DroppedBanks<'a> = BTreeSet<&'a str>;

/// `/documents` pages at `limit=100` by default and the largest bank has 22.
/// Asked for explicitly so pagination is not something a future corpus
/// discovers — and [`MigrateError::DocumentListTruncated`] is the backstop for
/// when it outgrows this too.
const DOCUMENTS_PAGE_LIMIT: &str = "1000";

/// How much of a 4xx/5xx body to keep in the error. Legacy's `detail` strings
/// are one sentence; this is generous and still bounded.
const ERROR_BODY_CHARS: usize = 500;

// ---------------------------------------------------------------------------
// the oracle: what `snapshot` records beside the archive
// ---------------------------------------------------------------------------

/// `GET /v1/default/banks/{bank}/stats` — **the count oracle.** The numbers
/// AC-3 compares against, frozen at snapshot time.
///
/// Deliberately *not* `deny_unknown_fields`, unlike everything in
/// [`super::archive`]: this is not the migration source, it is a measurement,
/// and refusing to snapshot because legacy's stats page grew a counter would
/// be strictness pointed at the wrong thing. `extra` keeps those fields
/// verbatim so `stats.json` still outlives the daemon intact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankStats {
    pub bank_id: String,
    pub total_nodes: i64,
    pub total_documents: i64,
    #[serde(default)]
    pub total_observations: i64,
    #[serde(default)]
    pub nodes_by_fact_type: BTreeMap<String, i64>,
    #[serde(default)]
    pub links_by_link_type: BTreeMap<String, i64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl BankStats {
    pub fn caused_by(&self) -> i64 {
        self.links_by_link_type
            .get("caused_by")
            .copied()
            .unwrap_or(0)
    }
}

/// One `/documents` row: the three fields the migration reads, plus every
/// other one kept verbatim.
///
/// `extra` is not tidiness. `/documents` answers ten fields and one of them —
/// **`document_metadata`** — has no counterpart anywhere in `schema.py`'s
/// `TransferDocument`, which makes it precisely the shape
/// `deny_unknown_fields` structurally cannot see: not an unknown field *in*
/// the archive, but a field the archive does not have. It is byte-identical to
/// `retain_params.metadata` in all 24 live documents, which is what D2 plans
/// to carry — and that equality is an assumption D2 depends on whose only
/// possible proof is this copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSummary {
    pub id: String,
    /// Verified equal to `sha256(original_text)` in 24/24 documents, and the
    /// same construction our own retain uses (`retain/mod.rs:146`). This is
    /// the document identity `documents::set_content_hash` will carry.
    pub content_hash: String,
    pub memory_unit_count: i64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ListDocumentsResponse {
    items: Vec<DocumentSummary>,
    /// The page carries its own truncation flag (`api/http.py:1564-1567`) and
    /// `limit` defaults to 100. Reading only `items` throws that away.
    total: i64,
}

#[derive(Debug, Deserialize)]
struct MemoriesListPage {
    total: i64,
}

#[derive(Debug, Deserialize)]
struct BankListResponse {
    banks: Vec<BankSummary>,
}

#[derive(Debug, Deserialize)]
struct BankSummary {
    bank_id: String,
}

/// Everything `snapshot` learned about one bank from the endpoints that are
/// *not* the archive, written to `stats.json` and consumed by
/// [`assert_integrity`], D2 and D3.
///
/// The plan calls this parameter `&Stats` after its headline member; it is a
/// superset, because three of the integrity checks (`content_hash`, the
/// per-document fact count, the invalidated-fact census) need `/documents` and
/// `/memories/list` and those must be frozen with the same provenance and the
/// same lifetime as `/stats` itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub bank_id: String,
    pub stats: BankStats,
    /// Empty for a dropped bank — see [`Stats::dropped`].
    #[serde(default)]
    pub documents: Vec<DocumentSummary>,
    /// `/documents`' own `total`, so a truncated page is visible as such.
    #[serde(default)]
    pub documents_total: i64,
    /// `GET /memories/list?limit=1` — live facts.
    #[serde(default)]
    pub memories_total: i64,
    /// `GET /memories/list?limit=1&state=invalidated` — the curation archive,
    /// a **different table** (`curation.py:141-143`). 0 in every bank today.
    ///
    /// There is no `state=valid` field here: `state` selects a table rather
    /// than adding a predicate, so unfiltered and `state=valid` are the same
    /// COUNT over `memory_units` and a `memories_valid` would have been
    /// `memories_total` by construction — a field D3 could gate on and always
    /// pass. Measured live, `bank-a`: 536 / 536 / 0.
    #[serde(default)]
    pub memories_invalidated: i64,
    /// True for a bank named in [`DroppedBanks`]: its `/stats` is frozen here (the
    /// zeroes that justified dropping it are evidence, and they die with
    /// :9077 otherwise) but it has no archive, so the archive↔oracle
    /// reconciliation in [`run`] must not expect one.
    #[serde(default)]
    pub dropped: bool,
}

// ---------------------------------------------------------------------------
// HTTP — the only place a request is built
// ---------------------------------------------------------------------------

/// The one request constructor in this module (Cross-PR rule 1). Takes a URL,
/// returns bytes, issues `GET`.
async fn get(client: &reqwest::Client, url: &Url) -> Result<Vec<u8>> {
    let http = |source| MigrateError::Http {
        url: url.to_string(),
        source,
    };
    let response = client.get(url.clone()).send().await.map_err(http)?;
    let status = response.status();
    if !status.is_success() {
        // Legacy puts the reason in the body (`{"detail": …}`,
        // `api/http.py:5849`); a bare status code sends the operator to the
        // daemon's log for something already in hand.
        let body = response.text().await.unwrap_or_default();
        return Err(MigrateError::HttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
            body: body.chars().take(ERROR_BODY_CHARS).collect(),
        });
    }
    response.bytes().await.map(|b| b.to_vec()).map_err(http)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &Url,
) -> Result<T> {
    let bytes = get(client, url).await?;
    serde_json::from_slice(&bytes).map_err(|source| MigrateError::JsonResponse {
        url: url.to_string(),
        source,
    })
}

/// Percent-encodes each path segment. Bank ids carry `::` and — for
/// `claude-code::bank e` — a space, which is not a legal path
/// character; `Url::path_segments_mut` is what makes that survive the wire
/// without hand-rolling an encoder.
fn endpoint(base: &str, segments: &[&str], query: &[(&str, &str)]) -> Url {
    let mut url = Url::parse(base).expect("the base is a valid absolute URL");
    url.path_segments_mut()
        .expect("the base has a path")
        .extend(segments);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query.iter().copied());
    }
    url
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

/// Reads legacy, writes `<out>/`, asserts, and returns the per-bank lines it
/// printed.
///
/// Nothing is asserted until everything is on disk. A failed run therefore
/// leaves the artifacts an operator needs to see *why* it failed, which is
/// worth more than the tidiness of refusing before writing — the whole run is
/// **1.62 s measured** (21 GETs, 1.94 MB zipped / 23 MB unpacked, four `unzip`
/// invocations and a 23 MB sha256 pass), and rerunning it costs nothing.
pub async fn run(out: &Path, dropped: &DroppedBanks<'_>) -> Result<Vec<String>> {
    run_from(LEGACY_BASE, out, dropped).await
}

/// [`run`] against an arbitrary base URL.
///
/// The override exists for one reason and it is written down in D1's own
/// deferred list: **`run` had no automated coverage at all.** Every pure
/// function under it was unit-tested, and the wiring between them — which is
/// where a one-directional reconciliation hid until code review found it —
/// was covered only by a manual run against the live daemon. `LEGACY_BASE`
/// stays the production constant and no caller outside a test passes anything
/// else.
pub async fn run_from(base: &str, out: &Path, dropped: &DroppedBanks<'_>) -> Result<Vec<String>> {
    std::fs::create_dir_all(out).map_err(|e| MigrateError::io(out, e))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|source| MigrateError::Http {
            url: base.to_string(),
            source,
        })?;

    // 1. The bank list, written verbatim. A dropped bank's hand-written mission
    //    is the one string that would otherwise be lost by not migrating it, so
    //    the file preserves every mission even for the banks that do not
    //    survive.
    let banks_url = endpoint(base, &["v1", "default", "banks"], &[]);
    let banks_bytes = get(&client, &banks_url).await?;
    write_file(&out.join("banks.json"), &banks_bytes)?;
    let banks: BankListResponse = serde_json::from_slice(&banks_bytes)
        .map_err(|e| MigrateError::json(banks_url.to_string(), e))?;

    assert_slugs_usable(banks.banks.iter().map(|b| b.bank_id.as_str()))?;

    let mut oracle: BTreeMap<String, Stats> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();

    for bank in &banks.banks {
        let id = bank.bank_id.as_str();
        let stats: BankStats = get_json(
            &client,
            &endpoint(base, &["v1", "default", "banks", id, "stats"], &[]),
        )
        .await?;

        if dropped.contains(id) {
            assert_dropped_bank_empty(id, &stats)?;
            lines.push(format!("drop {id}: empty, not migrated"));
            // Freeze its `/stats` anyway. The zeroes are the *evidence* for
            // the decision not to migrate it, and they stop existing when
            // Phase F retires :9077 — which is the same argument that made
            // the whole snapshot a file rather than a live read.
            oracle.insert(
                id.to_string(),
                Stats {
                    bank_id: id.to_string(),
                    stats,
                    documents: Vec::new(),
                    documents_total: 0,
                    memories_total: 0,
                    memories_invalidated: 0,
                    dropped: true,
                },
            );
            continue;
        }

        // 2. The content oracle. Written byte-for-byte as legacy produced it,
        //    then unpacked beside itself: the `.zip` is what `SHA256SUMS`
        //    anchors and what a reviewer can re-open, the directory is what
        //    D2/D3 read.
        let slug = slug(id);
        let zip_path = out.join(format!("{slug}.zip"));
        let zip_bytes = get(
            &client,
            &endpoint(
                base,
                &["v1", "default", "banks", id, "document-transfer"],
                &[("include_observations", "true")],
            ),
        )
        .await?;
        write_file(&zip_path, &zip_bytes)?;
        unpack(&zip_path, &out.join(&slug))?;

        let documents: ListDocumentsResponse = get_json(
            &client,
            &endpoint(
                base,
                &["v1", "default", "banks", id, "documents"],
                &[("limit", DOCUMENTS_PAGE_LIMIT)],
            ),
        )
        .await?;
        let live: MemoriesListPage = get_json(
            &client,
            &endpoint(
                base,
                &["v1", "default", "banks", id, "memories", "list"],
                &[("limit", "1")],
            ),
        )
        .await?;
        // `state=invalidated` reads `invalidated_memory_units`, a different
        // table (`curation.py:141-143`) — which is the only reason this GET
        // carries information. `state=valid` would re-count the table the
        // unfiltered call already counted.
        let invalidated: MemoriesListPage = get_json(
            &client,
            &endpoint(
                base,
                &["v1", "default", "banks", id, "memories", "list"],
                &[("limit", "1"), ("state", "invalidated")],
            ),
        )
        .await?;

        oracle.insert(
            id.to_string(),
            Stats {
                bank_id: id.to_string(),
                stats,
                documents: documents.items,
                documents_total: documents.total,
                memories_total: live.total,
                memories_invalidated: invalidated.total,
                dropped: false,
            },
        );
    }

    write_file(
        &out.join("stats.json"),
        &serde_json::to_vec_pretty(&oracle).expect("Stats serializes"),
    )?;

    // 3. Assert over what is on disk — the unpacked archive and a *reloaded*
    //    `stats.json`, not the responses just parsed. What D2 and D3 read is
    //    then exactly what was asserted, and the round trip through
    //    `BankStats`'s flattened `extra` map is exercised on every run rather
    //    than discovered by the importer.
    let oracle = super::load_stats(out)?;
    let archives = load_dir(out)?;
    for archive in &archives {
        let stats = oracle
            .get(&archive.bank_id)
            .ok_or_else(|| MigrateError::StatsMissing {
                bank: archive.bank_id.clone(),
            })?;
        assert_integrity(archive, stats)?;
        lines.push(integrity_line(archive, stats));
    }

    assert_every_bank_loaded(&oracle, &archives, out)?;

    write_sha256sums(out)?;
    Ok(lines)
}

/// The reverse of the loop above, and the one that catches something
/// *disappearing* rather than disagreeing.
///
/// [`load_dir`] recognises a bank archive by its `manifest.json`, so a
/// directory that ends up without one — an unexpected zip layout, a partial
/// write, an `unzip` that exits 0 having extracted nothing — is not seen at
/// all. Iterating archives → oracle can never notice: it would be one fewer
/// `ok` line, checksums written and verified, exit 0. `NoArchives` fires only
/// when *every* one vanishes.
fn assert_every_bank_loaded(
    oracle: &BTreeMap<String, Stats>,
    archives: &[BankArchive],
    dir: &Path,
) -> Result<()> {
    let loaded: BTreeSet<&str> = archives.iter().map(|a| a.bank_id.as_str()).collect();
    for (bank, stats) in oracle {
        if !stats.dropped && !loaded.contains(bank.as_str()) {
            return Err(MigrateError::ArchiveMissing {
                bank: bank.clone(),
                dir: dir.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// The line the manual verification pastes: the two coverage identities that
/// make the archive a complete migration source, with both sides shown.
fn integrity_line(archive: &BankArchive, stats: &Stats) -> String {
    format!(
        "ok   {}: {} facts + {} obs == {} nodes | causal {} == {} | docs {} == {} | live {} \
         invalidated {}",
        archive.bank_id,
        archive.manifest.fact_count,
        archive.manifest.observation_count,
        stats.stats.total_nodes,
        archive.causal_relations().count(),
        stats.stats.caused_by(),
        archive.manifest.document_count,
        stats.stats.total_documents,
        stats.memories_total,
        stats.memories_invalidated,
    )
}

/// Filesystem-safe name for a bank id. `claude-code::bank-a` →
/// `claude-code__bank-a`; `claude-code::bank e` →
/// `claude-code__bank_e`.
///
/// Lossy on purpose — the real id lives in `manifest.source_bank_id` and in
/// `stats.json`, both of which are what anything downstream reads. The one
/// hazard is two ids slugging to the same name and silently overwriting each
/// other's archive, which [`assert_no_slug_collision`] refuses.
pub fn slug(bank_id: &str) -> String {
    bank_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Refuses a slug that would collide with another bank's, and one that is not
/// a *new* directory under `--out`.
///
/// `.` and `..` are made entirely of characters [`slug`] passes through, so
/// they survive it unchanged, and an empty bank id yields `""` — for which
/// `out.join(slug)` is `out` itself. A bank id of `..` would therefore have
/// `unzip` extracting into the snapshot directory's **parent**.
fn assert_slugs_usable<'a>(bank_ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for id in bank_ids {
        let slug = slug(id);
        if matches!(slug.as_str(), "" | "." | "..") {
            return Err(MigrateError::UnusableSlug {
                bank: id.to_string(),
                slug,
            });
        }
        if let Some(other) = seen.insert(slug.clone(), id) {
            return Err(MigrateError::SlugCollision {
                a: other.to_string(),
                b: id.to_string(),
                slug,
            });
        }
    }
    Ok(())
}

/// `ponytail:` the archive is unpacked by the `unzip` the runbook already
/// calls, not by a DEFLATE decoder in this workspace.
///
/// The plan's §Workspace decision says "no ZIP code" and Phase D adds no
/// dependency, but `snapshot` still has to *read* the archive — every
/// integrity assertion below is about its contents. Shelling out is what
/// squares those: zero new crates, and the on-disk directory the runbook used
/// to produce by hand is produced here instead, so `import` and `verify` can
/// never be pointed at a snapshot nobody unpacked.
///
/// **Two ceilings, and the second is the one to know.**
///
/// 1. `unzip(1)` must be on PATH. The upgrade path is *not* as cheap as it
///    looks: `flate2` is already in `Cargo.lock` (via `hf-hub`'s `ureq`) but
///    it is a raw DEFLATE codec, and this is a ZIP **container** — so
///    replacing the shell-out needs a central-directory parser (~100 lines) or
///    the `zip` crate, which is not in the lock at all.
/// 2. **Archive path safety is `unzip`'s, not ours.** Nothing here inspects
///    entry names, so an archive containing `../…` or an absolute path is
///    handled by whatever binary answers to `unzip`. The Info-ZIP 6.00 on this
///    box strips a leading `/` and refuses `..` components — but that is a
///    property of this implementation, not of the interface, and the
///    archive's producer is a daemon we are about to retire. If the source
///    ever stops being a legacy daemon we control, this is the line that has
///    to become our own parse.
fn unpack(zip: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| MigrateError::io(dest, e))?;
    let status = std::process::Command::new("unzip")
        .args(["-q", "-o", "-d"])
        .arg(dest)
        .arg(zip)
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(MigrateError::Unpack {
            zip: zip.to_path_buf(),
            message: format!("unzip exited with {status}"),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(MigrateError::Unpack {
            zip: zip.to_path_buf(),
            message: "unzip(1) is not on PATH; unpack by hand with \
                      `python3 -m zipfile -e <zip> <dir>` and rerun"
                .to_string(),
        }),
        Err(e) => Err(MigrateError::Unpack {
            zip: zip.to_path_buf(),
            message: e.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// integrity
// ---------------------------------------------------------------------------

/// Every §Failure posture check that is about one bank's archive, in the order
/// that makes each failure legible.
///
/// The ordering is not cosmetic. `schema_version` comes first because a bumped
/// layout means nothing below is entitled to an opinion; the manifest's
/// self-consistency comes next because every count check after it reads the
/// manifest and would otherwise be reconciling against a number that is
/// already wrong.
///
/// Each check has its own error variant and its own test, because the point of
/// a refusal is to name the property that stopped holding. "Migration failed"
/// is a refusal nobody can act on.
pub fn assert_integrity(archive: &BankArchive, stats: &Stats) -> Result<()> {
    let bank = archive.bank_id.clone();
    let manifest = &archive.manifest;

    if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(MigrateError::UnsupportedSchemaVersion {
            bank,
            found: manifest.schema_version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }

    let counted = [
        (
            "document_count",
            manifest.document_count,
            archive.documents.len(),
        ),
        ("fact_count", manifest.fact_count, archive.fact_count()),
        (
            "observation_count",
            manifest.observation_count,
            archive.observations.len(),
        ),
    ];
    for (field, claimed, actual) in counted {
        if claimed != actual as i64 {
            return Err(MigrateError::ManifestCountMismatch {
                bank,
                field,
                manifest: claimed,
                actual: actual as i64,
            });
        }
    }

    // The reverse direction for documents. Everything below reconciles
    // archive → legacy, which catches a row that *disagrees* and misses one
    // that *disappears*: a `/documents` row or a `/stats.total_documents` with
    // no archive document behind it. Both numbers were already fetched.
    if stats.documents.len() as i64 != stats.documents_total {
        return Err(MigrateError::DocumentListTruncated {
            bank,
            items: stats.documents.len() as i64,
            total: stats.documents_total,
        });
    }
    if manifest.document_count != stats.stats.total_documents
        || manifest.document_count != stats.documents_total
    {
        return Err(MigrateError::DocumentCountMismatch {
            bank,
            archive: manifest.document_count,
            stats: stats.stats.total_documents,
            listed: stats.documents_total,
        });
    }

    let legacy_documents: BTreeMap<&str, &DocumentSummary> =
        stats.documents.iter().map(|d| (d.id.as_str(), d)).collect();

    for document in &archive.documents {
        let legacy = legacy_documents.get(document.id.as_str()).ok_or_else(|| {
            MigrateError::DocumentNotInLegacyList {
                bank: bank.clone(),
                document: document.id.clone(),
            }
        })?;

        // `_load_facts` is `ORDER BY document_id, created_at, id`
        // (`export.py:509`), a total order — so `fact_index` is stable
        // *provided no fact was deleted between snapshots*. This is that
        // proviso, checked rather than assumed.
        if legacy.memory_unit_count != document.facts.len() as i64 {
            return Err(MigrateError::DocumentFactCountMismatch {
                bank,
                document: document.id.clone(),
                archive: document.facts.len() as i64,
                legacy: legacy.memory_unit_count,
            });
        }

        let Some(original_text) = document.original_text.as_deref() else {
            return Err(MigrateError::MissingOriginalText {
                bank,
                document: document.id.clone(),
            });
        };
        let computed = sha256_hex(original_text.as_bytes());
        if computed != legacy.content_hash {
            return Err(MigrateError::ContentHashMismatch {
                bank,
                document: document.id.clone(),
                computed,
                legacy: legacy.content_hash.clone(),
            });
        }
    }

    // The coverage identity. Everything legacy counts as a node is either an
    // exported fact or an exported observation — measured exact in 4/4 banks.
    // A shortfall is most plausibly `_load_observations`' stale-source skip
    // (`export.py:466`), which logs and continues.
    if manifest.fact_count + manifest.observation_count != stats.stats.total_nodes {
        return Err(MigrateError::NodeCountMismatch {
            bank,
            facts: manifest.fact_count,
            observations: manifest.observation_count,
            total_nodes: stats.stats.total_nodes,
        });
    }

    let causal = archive.causal_relations().count() as i64;
    if causal != stats.stats.caused_by() {
        return Err(MigrateError::CausalCountMismatch {
            bank,
            archive: causal,
            stats: stats.stats.caused_by(),
        });
    }

    // Curation *moves* an invalidated fact into `invalidated_memory_units`
    // (`curation.py:11,141-143`) and `_load_facts` reads `memory_units`
    // (`export.py:489`) — so an invalidated fact cannot be exported, and the
    // exposure is that the curation archive is left behind with nothing
    // saying so. Do not silently drop: stop and let a human decide.
    if stats.memories_invalidated != 0 {
        return Err(MigrateError::InvalidatedFactPresent {
            bank,
            invalidated: stats.memories_invalidated,
        });
    }

    for (index, observation) in archive.observations.iter().enumerate() {
        if observation.sources.is_empty() {
            return Err(MigrateError::ObservationWithoutSources { bank, index });
        }
        if let Some(scopes) = &observation.observation_scopes {
            return Err(MigrateError::ObservationScopesUnsupported {
                bank,
                index,
                scopes: serde_json::to_string(scopes).unwrap_or_else(|_| "?".to_string()),
            });
        }
    }

    Ok(())
}

/// A bank named in [`DroppedBanks`] must still hold nothing.
///
/// Separate from [`assert_integrity`] because a dropped bank has no archive to
/// assert against — that is the whole point of dropping it.
pub fn assert_dropped_bank_empty(bank_id: &str, stats: &BankStats) -> Result<()> {
    if stats.total_nodes != 0 || stats.total_documents != 0 {
        return Err(MigrateError::DroppedBankNotEmpty {
            bank: bank_id.to_string(),
            nodes: stats.total_nodes,
            documents: stats.total_documents,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SHA256SUMS
// ---------------------------------------------------------------------------

/// `sha256sum(1)`-compatible: `<hex><two spaces><path relative to dir>`, one
/// line per file, sorted by path.
///
/// Compatible on purpose — the runbook's `sha256sum -c SHA256SUMS` is the
/// operator's independent check, and a format only our own verifier can read
/// would make that line a lie.
pub fn write_sha256sums(dir: &Path) -> Result<()> {
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_files(dir, dir, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let body: String = entries
        .iter()
        .map(|(path, hex)| format!("{hex}  {path}\n"))
        .collect();
    write_file(&dir.join("SHA256SUMS"), body.as_bytes())
}

/// Recomputes every hash in `SHA256SUMS` and fails on the first difference.
pub fn verify_sha256sums(dir: &Path) -> Result<()> {
    let path = dir.join("SHA256SUMS");
    let body = std::fs::read_to_string(&path).map_err(|e| MigrateError::io(&path, e))?;
    for (line_no, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (expected, relative) =
            line.split_once("  ")
                .ok_or_else(|| MigrateError::MalformedChecksums {
                    path: path.clone(),
                    message: format!("line {} is not '<hex>  <path>'", line_no + 1),
                })?;
        let file = dir.join(relative);
        if !file.is_file() {
            return Err(MigrateError::MissingFile { path: file });
        }
        let actual = sha256_hex(&std::fs::read(&file).map_err(|e| MigrateError::io(&file, e))?);
        if actual != expected {
            return Err(MigrateError::ChecksumMismatch {
                path: relative.to_string(),
                expected: expected.to_string(),
                actual,
            });
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::io(dir, e))?
        .map(|e| e.map(|e| e.path()).map_err(|e| MigrateError::io(dir, e)))
        .collect::<Result<Vec<_>>>()?;
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_files(root, &child, out)?;
            continue;
        }
        // The checksum file cannot list itself — but only *it*. Skipping the
        // name at any depth left a file legacy could legally emit inside a
        // bank directory unhashed and unverified, which is a hole of exactly
        // the shape this module exists against: not a mismatch, an absence.
        if child == root.join("SHA256SUMS") {
            continue;
        }
        let relative = child
            .strip_prefix(root)
            .expect("child is under root")
            .to_string_lossy()
            .into_owned();
        let hex = sha256_hex(&std::fs::read(&child).map_err(|e| MigrateError::io(&child, e))?);
        out.push((relative, hex));
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| MigrateError::io(parent, e))?;
    }
    std::fs::write(path, bytes).map_err(|e| MigrateError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::archive::load_bank_dir;
    use crate::migrate::test_support::{
        MutableSnapshot, edge_archive, real_fixture, real_scratch, real_stats,
    };

    fn real() -> (BankArchive, Stats) {
        let archives = load_dir(&real_fixture()).unwrap();
        let stats = real_stats();
        (archives.into_iter().next().unwrap(), stats)
    }

    #[test]
    fn the_real_fixture_passes_every_integrity_check() {
        let (archive, stats) = real();
        assert_integrity(&archive, &stats).expect("the committed slice reconciles");
        assert!(integrity_line(&archive, &stats).contains("86 facts + 79 obs == 165 nodes"));
    }

    // --- one test per named check ------------------------------------------

    #[test]
    fn a_bumped_schema_version_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_manifest(|m| m["schema_version"] = serde_json::json!(2));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::UnsupportedSchemaVersion { found: 2, .. })
        ));
    }

    #[test]
    fn a_manifest_fact_count_off_by_one_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_manifest(|m| m["fact_count"] = serde_json::json!(85));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ManifestCountMismatch {
                field: "fact_count",
                manifest: 85,
                actual: 86,
                ..
            })
        ));
    }

    #[test]
    fn a_manifest_observation_count_off_by_one_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_manifest(|m| m["observation_count"] = serde_json::json!(78));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ManifestCountMismatch {
                field: "observation_count",
                ..
            })
        ));
    }

    /// The coverage identity, broken from the `/stats` side so the manifest
    /// stays self-consistent and this check is the only one that can fire.
    #[test]
    fn a_node_count_that_does_not_cover_the_archive_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["stats"]["total_nodes"] = serde_json::json!(166));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::NodeCountMismatch {
                facts: 86,
                observations: 79,
                total_nodes: 166,
                ..
            })
        ));
    }

    #[test]
    fn a_causal_relation_count_mismatch_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot
            .edit_stats(|s| s["stats"]["links_by_link_type"]["caused_by"] = serde_json::json!(5));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::CausalCountMismatch {
                archive: 4,
                stats: 5,
                ..
            })
        ));
    }

    /// The census that carries information is `state=invalidated` — a
    /// different *table* (`curation.py:141-143`), not a predicate. The
    /// original check compared unfiltered against `state=valid`, which are the
    /// same COUNT over `memory_units`, so it compared a number with itself and
    /// could never fire. Verified live before this test existed: 536 / 536 / 0.
    #[test]
    fn an_invalidated_fact_stops_the_run() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["memories_invalidated"] = serde_json::json!(2));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::InvalidatedFactPresent { invalidated: 2, .. })
        ));
    }

    /// The deletion probe for the check above: with the assertion removed the
    /// fixture must stop passing. A census that equals another census by
    /// construction cannot do that, which is exactly how the first version got
    /// through review.
    #[test]
    fn the_invalidated_census_is_not_a_restatement_of_the_live_one() {
        let mut snapshot = MutableSnapshot::real();
        // Moving the *live* total does not touch it — the two numbers come
        // from different tables and are not derivable from each other.
        snapshot.edit_stats(|s| s["memories_total"] = serde_json::json!(1));
        assert!(
            snapshot.assert_integrity().is_ok(),
            "memories_total is recorded for D3, not gated on here"
        );
    }

    #[test]
    fn a_manifest_document_count_off_by_one_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_manifest(|m| m["document_count"] = serde_json::json!(2));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ManifestCountMismatch {
                field: "document_count",
                manifest: 2,
                actual: 1,
                ..
            })
        ));
    }

    /// The reverse direction: a `/documents` row or a `/stats` document with
    /// no archive document behind it. Every other document check runs
    /// archive → legacy and would see nothing.
    #[test]
    fn a_document_that_exists_only_in_stats_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["stats"]["total_documents"] = serde_json::json!(2));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::DocumentCountMismatch {
                archive: 1,
                stats: 2,
                listed: 1,
                ..
            })
        ));
    }

    /// `/documents` pages at `limit=100`; the response says so in its own
    /// `total`, which the first version of this struct discarded.
    #[test]
    fn a_truncated_documents_page_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["documents_total"] = serde_json::json!(3));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::DocumentListTruncated {
                items: 1,
                total: 3,
                ..
            })
        ));
    }

    /// `/documents` answers ten fields and `document_metadata` has no
    /// counterpart in `schema.py` — the one shape `deny_unknown_fields`
    /// structurally cannot see, so `stats.json` has to keep it.
    #[test]
    fn document_rows_keep_the_fields_the_archive_has_no_counterpart_for() {
        let stats = real_stats();
        let extra = &stats.documents[0].extra;
        assert!(
            extra.contains_key("document_metadata"),
            "got {:?}",
            extra.keys().collect::<Vec<_>>()
        );
        // And it round-trips, or freezing it buys nothing.
        let json = serde_json::to_string(&stats).unwrap();
        let back: Stats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.documents[0].extra, *extra);
    }

    #[test]
    fn a_content_hash_mismatch_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot
            .edit_stats(|s| s["documents"][0]["content_hash"] = serde_json::json!("0".repeat(64)));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ContentHashMismatch { .. })
        ));
    }

    /// The same check from the other side: legacy's hash is untouched and the
    /// text moves, which is what an edited or truncated `original_text` looks
    /// like.
    #[test]
    fn an_edited_original_text_is_caught_by_its_hash() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_document(|d| d["original_text"] = serde_json::json!("tampered"));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ContentHashMismatch { .. })
        ));
    }

    #[test]
    fn a_document_missing_from_the_legacy_list_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["documents"][0]["id"] = serde_json::json!("not-the-document"));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::DocumentNotInLegacyList { .. })
        ));
    }

    #[test]
    fn a_per_document_fact_count_disagreement_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_stats(|s| s["documents"][0]["memory_unit_count"] = serde_json::json!(87));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::DocumentFactCountMismatch {
                archive: 86,
                legacy: 87,
                ..
            })
        ));
    }

    #[test]
    fn an_observation_with_no_sources_is_refused() {
        let mut snapshot = MutableSnapshot::real();
        snapshot.edit_observations(|o| o[0]["sources"] = serde_json::json!([]));
        assert!(matches!(
            snapshot.assert_integrity(),
            Err(MigrateError::ObservationWithoutSources { index: 0, .. })
        ));
    }

    /// `original_text: null` is legal per `schema.py:125` and absent from all
    /// 24 live documents, so it lives in the synthetic `edge/` fixture.
    #[test]
    fn a_null_original_text_is_refused() {
        let (archive, stats) = edge_archive("edge::null-original-text");
        assert!(matches!(
            assert_integrity(&archive, &stats),
            Err(MigrateError::MissingOriginalText { .. })
        ));
    }

    /// Null in all 1,747 live observations, censused not sampled — and there
    /// is no MemGarden column for it, so a non-null value is a silent drop
    /// that `deny_unknown_fields` structurally cannot catch.
    #[test]
    fn a_non_null_observation_scope_is_refused() {
        let (archive, stats) = edge_archive("edge::observation-scopes");
        assert!(matches!(
            assert_integrity(&archive, &stats),
            Err(MigrateError::ObservationScopesUnsupported { index: 0, .. })
        ));
    }

    /// The shapes that are legal per `schema.py` but absent from today's
    /// corpus must be *accepted*, not refused — an empty `context`, an
    /// out-of-range `target_fact_index` (D2's pre-write check, not D1's), and
    /// duplicate `(document_id, fact_index)` source pairs.
    #[test]
    fn legal_but_absent_shapes_are_accepted() {
        let (archive, stats) = edge_archive("edge::legal-but-absent");
        assert_integrity(&archive, &stats).expect("legal shapes must not be refused here");

        let facts = &archive.documents[0].facts;
        assert_eq!(facts[0].context.as_deref(), Some(""));
        assert_eq!(facts[1].causal_relations[0].target_fact_index, 999);
        let sources = &archive.observations[0].sources;
        assert_eq!(sources.len(), 3);
        assert_eq!(
            sources.iter().collect::<BTreeSet<_>>().len(),
            2,
            "two distinct pairs out of three — the shape behind the 2,114 gate"
        );
    }

    /// A snapshotted bank whose archive did not load back. The failure this
    /// guards is silence: without it the run drops a bank, writes and verifies
    /// checksums, and exits 0.
    #[test]
    fn a_snapshotted_bank_with_no_archive_is_refused() {
        let dir = real_fixture();
        let archives = load_dir(&dir).unwrap();
        let mut oracle = super::super::load_stats(&dir).unwrap();
        assert_every_bank_loaded(&oracle, &archives, &dir).expect("as committed");

        // A second bank in `stats.json` that no directory backs.
        let mut ghost = oracle.values().next().unwrap().clone();
        ghost.bank_id = "claude-code::vanished".to_string();
        oracle.insert(ghost.bank_id.clone(), ghost);
        assert!(matches!(
            assert_every_bank_loaded(&oracle, &archives, &dir),
            Err(MigrateError::ArchiveMissing { .. })
        ));

        // …unless it was deliberately dropped, which is why the flag exists.
        oracle.get_mut("claude-code::vanished").unwrap().dropped = true;
        assert_every_bank_loaded(&oracle, &archives, &dir).expect("dropped banks have no archive");
    }

    #[test]
    fn a_dropped_bank_that_grew_content_is_refused() {
        let empty = BankStats {
            bank_id: "codex".to_string(),
            total_nodes: 0,
            total_documents: 0,
            total_observations: 0,
            nodes_by_fact_type: BTreeMap::new(),
            links_by_link_type: BTreeMap::new(),
            extra: BTreeMap::new(),
        };
        assert_dropped_bank_empty("codex", &empty).expect("still empty");

        let grown = BankStats {
            total_nodes: 3,
            ..empty
        };
        assert!(matches!(
            assert_dropped_bank_empty("codex", &grown),
            Err(MigrateError::DroppedBankNotEmpty {
                nodes: 3,
                documents: 0,
                ..
            })
        ));
    }

    // --- SHA256SUMS ---------------------------------------------------------

    #[test]
    fn the_committed_checksums_verify() {
        verify_sha256sums(&real_fixture()).expect("the fixture's own SHA256SUMS");
    }

    #[test]
    fn a_flipped_byte_is_detected() {
        let scratch = real_scratch();
        let target = scratch
            .path()
            .join("claude-code__bank-a/manifest.json");
        let mut bytes = std::fs::read(&target).unwrap();
        // A byte inside the exported_at timestamp: still valid JSON, still
        // parses, and changes nothing a count check would notice.
        let at = bytes.iter().position(|b| *b == b'2').unwrap();
        bytes[at] = b'3';
        std::fs::write(&target, &bytes).unwrap();

        assert!(matches!(
            verify_sha256sums(scratch.path()),
            Err(MigrateError::ChecksumMismatch { .. })
        ));
    }

    /// Also pins the format against `sha256sum(1)`: the committed file was
    /// produced by coreutils, so a byte-identical rewrite here is what makes
    /// the runbook's `sha256sum -c SHA256SUMS` line true rather than
    /// aspirational.
    /// Only the *root* checksum file is excluded. A file legacy could legally
    /// emit under a bank directory with that name used to be skipped at any
    /// depth — unhashed, unverified, and invisible.
    #[test]
    fn a_sha256sums_below_the_root_is_still_checksummed() {
        let scratch = real_scratch();
        let nested = scratch
            .path()
            .join("claude-code__bank-a/SHA256SUMS");
        std::fs::write(&nested, b"legacy could legally emit this\n").unwrap();
        write_sha256sums(scratch.path()).unwrap();
        let body = std::fs::read_to_string(scratch.path().join("SHA256SUMS")).unwrap();
        assert!(
            body.contains("claude-code__bank-a/SHA256SUMS"),
            "a nested checksum-named file must be covered:\n{body}"
        );
        assert!(
            !body.lines().any(|l| l.ends_with("  SHA256SUMS")),
            "and the root one still cannot list itself"
        );
        verify_sha256sums(scratch.path()).unwrap();
        std::fs::write(&nested, b"flipped\n").unwrap();
        assert!(matches!(
            verify_sha256sums(scratch.path()),
            Err(MigrateError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn write_sha256sums_round_trips_and_never_lists_itself() {
        let scratch = real_scratch();
        write_sha256sums(scratch.path()).unwrap();
        let body = std::fs::read_to_string(scratch.path().join("SHA256SUMS")).unwrap();
        assert!(!body.contains("SHA256SUMS"));
        assert_eq!(
            body.lines().count(),
            6,
            "3 archive files + banks.json + stats.json + README.md"
        );
        assert_eq!(
            body,
            std::fs::read_to_string(real_fixture().join("SHA256SUMS")).unwrap(),
            "our format must be sha256sum(1)'s, byte for byte"
        );
        verify_sha256sums(scratch.path()).unwrap();
    }

    #[test]
    fn a_listed_file_that_disappeared_is_reported_as_missing() {
        let scratch = real_scratch();
        std::fs::remove_file(scratch.path().join("banks.json")).unwrap();
        assert!(matches!(
            verify_sha256sums(scratch.path()),
            Err(MigrateError::MissingFile { .. })
        ));
    }

    #[test]
    fn a_malformed_checksum_line_is_reported_as_such() {
        let scratch = real_scratch();
        std::fs::write(scratch.path().join("SHA256SUMS"), "not a checksum line\n").unwrap();
        assert!(matches!(
            verify_sha256sums(scratch.path()),
            Err(MigrateError::MalformedChecksums { .. })
        ));
    }

    // --- slugs and URLs -----------------------------------------------------

    #[test]
    fn bank_ids_slug_to_filesystem_safe_names() {
        assert_eq!(
            slug("claude-code::bank-a"),
            "claude-code__bank-a"
        );
        assert_eq!(
            slug("claude-code::bank e"),
            "claude-code__bank_e"
        );
        assert_eq!(slug("codex"), "codex");
    }

    #[test]
    fn two_bank_ids_that_slug_alike_are_refused_rather_than_overwritten() {
        // `a:b` and `a b` both slug to `a_b`, and the second archive written
        // would silently replace the first.
        assert!(matches!(
            assert_slugs_usable(["a:b", "a b"].into_iter()),
            Err(MigrateError::SlugCollision { .. })
        ));
        // The shapes a real drop set takes, including the one bank id with a
        // space in it that motivated `slug` in the first place.
        assert!(
            assert_slugs_usable(
                ["claude-code::a", "claude-code::B C", "bare"].into_iter()
            )
            .is_ok()
        );
    }

    /// `.` and `..` are made of characters `slug` passes through, so they
    /// survive it — and `out.join("..")` is the snapshot directory's parent,
    /// which is where `unzip` would then extract.
    #[test]
    fn a_bank_id_that_slugs_to_a_path_traversal_is_refused() {
        for bad in ["", ".", ".."] {
            assert_eq!(slug(bad), bad, "slug passes {bad:?} through unchanged");
            assert!(
                matches!(
                    assert_slugs_usable([bad].into_iter()),
                    Err(MigrateError::UnusableSlug { .. })
                ),
                "{bad:?} was accepted"
            );
        }
    }

    /// The bank id goes into a path segment, and one of the eight has a space
    /// in it.
    #[test]
    fn endpoint_percent_encodes_the_bank_id() {
        let url = endpoint(
            LEGACY_BASE,
            &[
                "v1",
                "default",
                "banks",
                "claude-code::bank e",
                "stats",
            ],
            &[],
        );
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9077/v1/default/banks/claude-code::bank%20e/stats"
        );
    }

    #[test]
    fn endpoint_appends_the_query_the_archive_needs() {
        let url = endpoint(
            LEGACY_BASE,
            &["v1", "default", "banks", "b", "document-transfer"],
            &[("include_observations", "true")],
        );
        assert_eq!(url.query(), Some("include_observations=true"));
    }

    #[test]
    fn load_bank_dir_reads_the_bank_id_from_the_manifest_not_the_directory() {
        let archive = load_bank_dir(&real_fixture().join("claude-code__bank-a")).unwrap();
        assert_eq!(archive.bank_id, "claude-code::bank-a");
    }
}

/// `run()`'s own coverage, which D1 shipped without.
///
/// Every pure function under `run` had a test; the *wiring* between them had
/// none, and that is where the one-directional reconciliation lived until code
/// review found it. The stub below is a real `axum` server answering the five
/// real URLs — built with [`endpoint`] itself, so a change to how a bank id is
/// encoded breaks the test rather than passing it — serving the committed
/// fixtures back as legacy would.
#[cfg(test)]
mod run_tests {
    use super::*;
    use crate::migrate::test_support::fixture;

    const JCODE: &str = "claude-code::bank-a";
    const CMS: &str = "claude-code::bank-b";

    /// A stub legacy daemon: a routing table keyed on the exact URLs
    /// [`endpoint`] builds, and nothing else answered.
    struct Legacy {
        base: String,
        _scratch: tempfile::TempDir,
    }

    impl Legacy {
        /// `banks` is `(bank id, fixture directory, bank slug)`. A `None`
        /// directory serves a ZIP with no `manifest.json` in it — the shape
        /// `load_dir` skips **in silence**.
        async fn start(banks: &[(&str, Option<&str>)]) -> Legacy {
            let scratch = tempfile::tempdir().unwrap();
            let mut routes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            let mut listed = Vec::new();

            for (index, (bank, from)) in banks.iter().enumerate() {
                let (stats, documents, zip) = match from {
                    Some(from) => Legacy::bank_from(scratch.path(), index, bank, from),
                    None => Legacy::bank_without_a_manifest(scratch.path(), index, bank),
                };
                listed.push(serde_json::json!({"bank_id": bank, "name": bank}));
                routes.insert(key(bank, &["stats"], &[]), stats);
                routes.insert(
                    key(
                        bank,
                        &["document-transfer"],
                        &[("include_observations", "true")],
                    ),
                    zip,
                );
                routes.insert(
                    key(bank, &["documents"], &[("limit", DOCUMENTS_PAGE_LIMIT)]),
                    documents,
                );
                routes.insert(
                    key(bank, &["memories", "list"], &[("limit", "1")]),
                    br#"{"total":0}"#.to_vec(),
                );
                routes.insert(
                    key(
                        bank,
                        &["memories", "list"],
                        &[("limit", "1"), ("state", "invalidated")],
                    ),
                    br#"{"total":0}"#.to_vec(),
                );
            }
            routes.insert(
                "/v1/default/banks".to_string(),
                serde_json::to_vec(&serde_json::json!({"banks": listed})).unwrap(),
            );

            let routes = std::sync::Arc::new(routes);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let routes = routes.clone();
                async move {
                    let key = match uri.query() {
                        Some(q) => format!("{}?{q}", uri.path()),
                        None => uri.path().to_string(),
                    };
                    match routes.get(&key) {
                        Some(body) => (axum::http::StatusCode::OK, body.clone()),
                        None => (
                            axum::http::StatusCode::NOT_FOUND,
                            format!(r#"{{"detail":"no stub route for {key}"}}"#).into_bytes(),
                        ),
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            Legacy {
                base,
                _scratch: scratch,
            }
        }

        /// `/stats`, `/documents` and the transfer ZIP for one bank, all
        /// derived from a committed fixture so the numbers reconcile.
        fn bank_from(
            scratch: &Path,
            index: usize,
            bank: &str,
            from: &str,
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let dir = fixture(from);
            let recorded: BTreeMap<String, Stats> =
                super::super::archive::read_json(&dir.join("stats.json")).unwrap();
            let recorded = &recorded[bank];
            let slug = slug(bank);
            (
                serde_json::to_vec(&recorded.stats).unwrap(),
                serde_json::to_vec(&serde_json::json!({
                    "items": recorded.documents,
                    "total": recorded.documents_total,
                }))
                .unwrap(),
                zip_dir(&dir.join(&slug), &scratch.join(format!("{index}.zip"))),
            )
        }

        /// The failure `assert_every_bank_loaded` exists for: a ZIP that
        /// unpacks cleanly and leaves no `manifest.json`, so `load_dir` never
        /// sees the bank and reconciling archive -> oracle cannot notice.
        fn bank_without_a_manifest(
            scratch: &Path,
            index: usize,
            bank: &str,
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let empty = scratch.join(format!("empty-{index}"));
            std::fs::create_dir_all(&empty).unwrap();
            std::fs::write(empty.join("notes.txt"), b"not an archive\n").unwrap();
            (
                serde_json::to_vec(&serde_json::json!({
                    "bank_id": bank, "total_nodes": 1, "total_documents": 1,
                    "total_observations": 0, "nodes_by_fact_type": {"world": 1},
                    "links_by_link_type": {"caused_by": 0},
                }))
                .unwrap(),
                serde_json::to_vec(&serde_json::json!({"items": [], "total": 0})).unwrap(),
                zip_dir(&empty, &scratch.join(format!("{index}.zip"))),
            )
        }
    }

    /// The routing key for one bank endpoint, built with [`endpoint`] itself
    /// — so a change to how a bank id is percent-encoded breaks these tests
    /// rather than quietly passing them.
    fn key(bank: &str, tail: &[&str], query: &[(&str, &str)]) -> String {
        let mut segments = vec!["v1", "default", "banks", bank];
        segments.extend_from_slice(tail);
        let url = endpoint("http://stub", &segments, query);
        match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        }
    }

    /// `zip(1)`, for the same reason `unpack` shells out to `unzip(1)`: no
    /// crate in this workspace speaks the ZIP container, and a test is not the
    /// place to add one.
    fn zip_dir(dir: &Path, zip: &Path) -> Vec<u8> {
        let status = std::process::Command::new("zip")
            .args(["-q", "-r", "-X"])
            .arg(zip)
            .arg(".")
            .current_dir(dir)
            .status()
            .expect("zip(1) on PATH, as unzip(1) already is");
        assert!(status.success(), "zip exited with {status}");
        std::fs::read(zip).unwrap()
    }

    #[tokio::test]
    async fn run_reads_five_endpoints_freezes_them_and_reconciles() {
        let legacy = Legacy::start(&[(JCODE, Some("real")), (CMS, Some("real-cms"))]).await;
        let out = tempfile::tempdir().unwrap();
        let lines = run_from(&legacy.base, out.path(), &DroppedBanks::new())
            .await
            .expect("reconciles");

        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("86 facts + 79 obs == 165 nodes"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("70 facts + 65 obs == 135 nodes"))
        );

        // Everything downstream reads is on disk, and the checksums cover it.
        for relative in [
            "banks.json",
            "stats.json",
            "SHA256SUMS",
            "claude-code__bank-a.zip",
            "claude-code__bank-a/manifest.json",
            "claude-code__bank-b/observations.json",
        ] {
            assert!(out.path().join(relative).is_file(), "missing {relative}");
        }
        verify_sha256sums(out.path()).expect("the snapshot verifies itself");

        // The frozen oracle round-trips through `BankStats`'s flattened map,
        // which is what `import` and `verify` will read rather than the
        // responses just parsed.
        let oracle = crate::migrate::load_stats(out.path()).unwrap();
        assert_eq!(oracle[JCODE].stats.total_nodes, 165);
        assert_eq!(oracle[CMS].documents.len(), 1);
    }

    /// The reconciliation that runs oracle -> archive, and the only one that
    /// can see a bank *disappear*. Without it this run is one fewer `ok` line,
    /// checksums written and verified, exit 0.
    #[tokio::test]
    async fn a_bank_whose_archive_never_arrived_fails_the_run() {
        let legacy = Legacy::start(&[(JCODE, Some("real")), (CMS, None)]).await;
        let out = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_from(&legacy.base, out.path(), &DroppedBanks::new()).await,
            Err(MigrateError::ArchiveMissing { .. })
        ));
    }

    /// Legacy answers 4xx with `{"detail": …}` (`api/http.py:5849`), so the
    /// body is the diagnosis and a bare status code is not.
    #[tokio::test]
    async fn an_endpoint_that_answers_404_names_the_url_and_the_body() {
        let legacy = Legacy::start(&[]).await;
        let out = tempfile::tempdir().unwrap();
        // `/v1/default/banks` is the one route an empty stub still serves, so
        // the 404 has to come from somewhere else: point the run at a path
        // that has no route at all.
        let err = run_from(&format!("{}/nope", legacy.base), out.path(), &DroppedBanks::new())
            .await
            .unwrap_err();
        let MigrateError::HttpStatus { status, body, .. } = err else {
            panic!("expected an HTTP status error, got {err}");
        };
        assert_eq!(status, 404);
        assert!(body.contains("no stub route"), "the body is the diagnosis");
    }
}
