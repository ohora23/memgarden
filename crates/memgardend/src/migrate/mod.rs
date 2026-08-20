//! MG-1 — moving four legacy Hindsight banks into this store.
//!
//! The whole phase rests on a finding that removes most of its hard questions:
//! **legacy already ships its own migration format.** `GET
//! /v1/default/banks/{bank}/document-transfer` returns a ZIP of facts,
//! observations, causal relations and documents with *"embeddings and database
//! ids not included — importing re-embeds with the target bank's model and
//! re-resolves entities"* (`hindsight-api-slim/hindsight_api/api/http.py:6795`;
//! implementation `engine/transfer/export.py:171`, types
//! `engine/transfer/schema.py:46-130`). Carrying the facts and re-deriving
//! everything else is therefore the *supported* path, not a compromise we
//! invented.
//!
//! D1 is the instrument and lands before anything writes a row: [`snapshot`]
//! reads legacy over HTTP, freezes the archive on disk, and refuses on any of
//! the integrity properties measured true today (§Failure posture of
//! `.omc/plans/phase-d-impl.md`). D2 adds `import`, D3 adds `verify`.
//!
//! **The failure this module exists to prevent is silent partial success** — a
//! migration that reports 5,287 nodes and has quietly dropped a field. Hence
//! [`archive`]'s `deny_unknown_fields` on every struct and one *named* error
//! per integrity check below: a refusal that cannot say which property broke
//! is a refusal nobody acts on.

pub mod archive;
pub mod import;
pub mod snapshot;
pub mod verify;

use std::path::PathBuf;

/// Every way D1 can refuse.
///
/// The integrity variants are one-per-check on purpose. They are the
/// acceptance criteria of this PR, each is asserted true against the live
/// corpus today, and each has a test that breaks exactly it — so a future
/// refusal names the property that stopped holding instead of "migration
/// failed".
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The one place a transport failure surfaces. `snapshot` issues GETs
    /// only (Cross-PR rule 1) and does not retry: the whole read is under two
    /// seconds, so "run it again" is cheaper than a resume path.
    ///
    /// Keeps the `reqwest::Error` as a `#[source]` rather than flattening it
    /// to a string. `reqwest`'s own `Display` prints only kind and URL — the
    /// part an operator needs (`Connection refused (os error 111)`, a DNS
    /// failure, a timeout) lives in *its* source, and a `to_string()` at
    /// construction discards that before `mg_migrate`'s cause walk can reach
    /// it.
    #[error("GET {url}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// Legacy answers 4xx/5xx with `{"detail": …}` (`api/http.py:5849`), so
    /// the body is the diagnosis and the status alone is not.
    #[error("{url} answered HTTP {status}: {body}")]
    HttpStatus {
        url: String,
        status: u16,
        body: String,
    },

    /// A response that is not the JSON we expected. Distinct from
    /// [`MigrateError::Json`], which names a *file*: putting a URL in a
    /// `PathBuf` makes a wire problem read as a disk problem.
    #[error("{url}: {source}")]
    JsonResponse {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    /// No crate in this workspace inflates a DEFLATE stream and Phase D adds
    /// no dependency, so the archive is unpacked by the `unzip` the runbook
    /// already calls. See `snapshot::unpack`.
    #[error("unpacking {zip}: {message}")]
    Unpack { zip: PathBuf, message: String },

    /// Two bank ids that differ only in characters the filesystem cannot hold
    /// would overwrite each other's archive. Refuse rather than lose one.
    #[error("bank ids {a:?} and {b:?} both slug to {slug:?}")]
    SlugCollision { a: String, b: String, slug: String },

    /// A slug that is not a *new* directory under `--out`. `.` and `..`
    /// survive [`snapshot::slug`] unchanged (both are made of characters it
    /// passes through) and an empty bank id slugs to `""`, so `out.join(slug)`
    /// would be the snapshot directory itself or its **parent** — and `unzip`
    /// would extract there.
    #[error("bank id {bank:?} slugs to {slug:?}, which is not a directory we may create")]
    UnusableSlug { bank: String, slug: String },

    #[error("{path} is missing from the snapshot")]
    MissingFile { path: PathBuf },

    #[error("no bank archive found under {dir} (expected <bank-slug>/manifest.json)")]
    NoArchives { dir: PathBuf },

    #[error("{path}: {message}")]
    MalformedChecksums { path: PathBuf, message: String },

    /// The async export flow (`snapshot::archive`) failed somewhere that is
    /// not a transport error and not an HTTP status: a completed operation
    /// with no `download_url`, an operation the daemon reports as failed, or
    /// a poll budget that ran out. Its own variant rather than a reused one,
    /// because "the export never produced an archive" and "the archive is
    /// corrupt" send an operator to different places.
    #[error("export: {0}")]
    Export(String),

    /// `SHA256SUMS` did not verify — the frozen archive is not the archive on
    /// disk, so nothing downstream may treat it as the oracle.
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("{bank}: no /stats recorded for this bank")]
    StatsMissing { bank: String },

    /// The mirror of [`MigrateError::StatsMissing`], and the one that matters:
    /// a bank was snapshotted but no archive came back for it.
    ///
    /// `load_dir` recognises a bank archive by its `manifest.json`
    /// (`archive.rs`), so a directory that ends up without one — an unexpected
    /// zip layout, a partial write, an `unzip` that exits 0 having extracted
    /// nothing — is simply **not seen**. Reconciling only archive → oracle
    /// leaves that silent: one fewer `ok` line, checksums written and
    /// verified, exit 0. `NoArchives` fires only when *all* of them vanish.
    #[error("{bank}: snapshotted, but no archive was loaded back from {dir}")]
    ArchiveMissing { bank: String, dir: PathBuf },

    // --- integrity: one variant per §Failure posture check ------------------
    /// `manifest.schema_version` is the archive's version contract
    /// (`schema.py:23`). A bump means the layout changed incompatibly and this
    /// parser is no longer entitled to an opinion about the bytes.
    #[error("{bank}: archive schema_version is {found}, this binary reads {supported}")]
    UnsupportedSchemaVersion {
        bank: String,
        found: i64,
        supported: i64,
    },

    /// The manifest disagrees with the files beside it. Checked first because
    /// every count assertion below reads the manifest and would otherwise be
    /// asserting against a number that is already wrong.
    #[error("{bank}: manifest.{field} is {manifest} but the archive holds {actual}")]
    ManifestCountMismatch {
        bank: String,
        field: &'static str,
        manifest: i64,
        actual: i64,
    },

    /// `_load_facts` orders by `(document_id, created_at, id)`
    /// (`export.py:509`), a total order — so `fact_index` is stable *provided
    /// no fact was deleted between snapshots*. This is that proviso, asserted
    /// against `/documents`' own `memory_unit_count` rather than assumed.
    #[error("{bank}: document {document} carries {archive} facts, /documents reports {legacy}")]
    DocumentFactCountMismatch {
        bank: String,
        document: String,
        archive: i64,
        legacy: i64,
    },

    /// The coverage identity: everything legacy counts as a node is either an
    /// exported fact or an exported observation. Measured exact in 4/4 banks.
    /// A shortfall means the export skipped content — most plausibly
    /// `_load_observations`' stale-source skip (`export.py:466`), which logs
    /// and continues.
    #[error("{bank}: {facts} facts + {observations} observations != {total_nodes} /stats nodes")]
    NodeCountMismatch {
        bank: String,
        facts: i64,
        observations: i64,
        total_nodes: i64,
    },

    /// `caused_by` is the only authored link type and the only one the
    /// migration copies rather than re-derives, so the archive must carry all
    /// of them. Measured exact in 4/4 banks.
    #[error("{bank}: archive holds {archive} causal relations, /stats reports {stats}")]
    CausalCountMismatch {
        bank: String,
        archive: i64,
        stats: i64,
    },

    /// **An invalidated fact exists, and the migration would leave it behind.**
    ///
    /// The direction here is the opposite of what this PR first claimed, and
    /// the wrong explanation was the more dangerous half of the bug.
    /// `state` on `/memories/list` selects a **table**, not a predicate
    /// (`engine/memories/pg/curation.py:141-143`: *"Invalidated facts live in
    /// a separate archive table; pick the source accordingly"*), and curation
    /// *moves* the row out of `memory_units` into `invalidated_memory_units`.
    /// `_load_facts` reads `memory_units` (`export.py:489`), so an invalidated
    /// fact **cannot** be exported — the exposure is that the curation archive
    /// is silently dropped, not that a dead fact arrives alive.
    ///
    /// Which also means the original check could never fire: unfiltered and
    /// `state=valid` are the same COUNT over the same table, so it compared a
    /// number with itself. Verified live: 536 / 536 / 0 for `bank-a`.
    /// The census that carries information is `state=invalidated`.
    #[error(
        "{bank}: {invalidated} invalidated fact(s) exist and the transfer archive cannot carry them"
    )]
    InvalidatedFactPresent { bank: String, invalidated: i64 },

    /// `schema.py:125` types `original_text` as `str | None` and 24/24 are
    /// non-null today — precisely the assumption that rots. `content_hash`,
    /// the document identity the import dedups on, cannot be computed without
    /// it.
    #[error("{bank}: document {document} has a null original_text")]
    MissingOriginalText { bank: String, document: String },

    /// The document is in the archive but not in `/documents`, so there is no
    /// `content_hash` to check it against.
    #[error("{bank}: document {document} is in the archive but not in /documents")]
    DocumentNotInLegacyList { bank: String, document: String },

    /// `/documents` returned fewer rows than it says exist — it pages at
    /// `limit=100` by default (`openapi.json`) and answers
    /// `{items, total, limit, offset}` (`api/http.py:1564-1567`). The response
    /// carries its own truncation flag; reading only `items` throws it away
    /// and turns pagination into a confusing per-document
    /// [`MigrateError::DocumentNotInLegacyList`].
    #[error("{bank}: /documents returned {items} of {total} rows")]
    DocumentListTruncated {
        bank: String,
        items: i64,
        total: i64,
    },

    /// The three independent document counts disagree.
    ///
    /// Every other reconciliation in this module runs archive → legacy, which
    /// catches a row that *disagrees* and misses one that *disappears*. This
    /// is the reverse direction for documents: a `/documents` row or a
    /// `/stats.total_documents` with no archive document behind it.
    #[error(
        "{bank}: {archive} documents in the archive, {stats} in /stats, {listed} in /documents"
    )]
    DocumentCountMismatch {
        bank: String,
        archive: i64,
        stats: i64,
        listed: i64,
    },

    /// `sha256(original_text) == content_hash` held 24/24 when measured, and
    /// it is the same construction our own retain uses
    /// (`crates/memgardend/src/retain/mod.rs:146`). It is the key document
    /// identity and idempotence rest on, so it is verified rather than trusted.
    #[error("{bank}: document {document} hashes to {computed}, legacy recorded {legacy}")]
    ContentHashMismatch {
        bank: String,
        document: String,
        computed: String,
        legacy: String,
    },

    /// An observation with no sources has lost its provenance: `node_sources`
    /// would be empty and `proof_count` would derive to 0
    /// (`consolidate.rs:658-666`). 0 of 1,747 today.
    #[error("{bank}: observation #{index} has an empty sources array")]
    ObservationWithoutSources { bank: String, index: usize },

    /// Null in all 1,747 observations, censused not sampled — and there is no
    /// MemGarden column for it, so a non-null value is a silent drop that
    /// `deny_unknown_fields` structurally cannot catch (the field is known,
    /// just unused).
    #[error("{bank}: observation #{index} has observation_scopes {scopes}, which we cannot store")]
    ObservationScopesUnsupported {
        bank: String,
        index: usize,
        scopes: String,
    },

    /// The four zero-content banks are deliberately not migrated (§What AC-3
    /// must mean now). "Nothing to lose" is only true while it is true, and
    /// two of the four are live directories — so it is re-checked at every
    /// snapshot rather than noted once.
    #[error(
        "{bank} was to be dropped as empty but now holds {nodes} nodes / {documents} documents"
    )]
    DroppedBankNotEmpty {
        bank: String,
        nodes: i64,
        documents: i64,
    },

    // --- import: the guards (§Failure posture, "`import` refuses when") -----
    /// A daemon is listening **and** the target is the database it holds open.
    /// See `import::assert_daemon_not_holding` for why both halves are
    /// required — the plan states only the first, and only the first would
    /// refuse every zero-downtime rehearsal the runbook asks for.
    #[error("a daemon is listening on {bind} and holds {db}; stop it or import into another --db")]
    DaemonListening { bind: String, db: PathBuf },

    #[error("{bind:?} is not a socket address this binary can probe")]
    UnparseableBind { bind: String },

    /// `Db::open` migrates forward but never backward: a database written by a
    /// newer binary keeps its `user_version` and every migration entry skips
    /// (`migrate.rs:44-48`), so an import would write into a schema it does
    /// not know.
    #[error("database is at schema_version {found}, this binary writes {supported}")]
    SchemaVersionMismatch { found: i64, supported: i64 },

    /// An import must start from an empty bank or it is not reproducible —
    /// `recall_bench.rs:180-186`'s rule, moved from the database to the bank.
    #[error("{bank} already holds {nodes} nodes; pass --replace to purge it first")]
    BankNotEmpty { bank: String, nodes: i64 },

    /// The partial-bank guarantee. There is no per-bank transaction
    /// (§Binding decisions #5), so a failed run leaves rows behind — and the
    /// marker is what stops them being mistaken for a finished import.
    #[error("{bank} carries an mg_import marker still at 'running'; only --replace may reuse it")]
    ImportInProgress { bank: String },

    /// `target_fact_index` is an ordinal into the **same document's** `facts`
    /// array (`schema.py:35-43`), typed `int` — so a negative or past-the-end
    /// value parses. Rejected before any write, because
    /// `graph::insert_links` would otherwise fail on a foreign key naming
    /// neither the document nor the fact.
    #[error(
        "{bank}: document {document} fact #{fact_index} points at causal target {target}, \
         but the document has {facts} facts"
    )]
    CausalTargetOutOfRange {
        bank: String,
        document: String,
        fact_index: usize,
        target: i64,
        facts: i64,
    },

    /// An observation source that names no fact in the archive.
    /// `insert_observation` filters unresolvable ids in SQL and drops them
    /// **silently** (`consolidate.rs:111-114`) — correct for the daemon, and a
    /// silent loss of proof here.
    #[error(
        "{bank}: observation #{index} sources ({document}, {fact_index}), which is not in the archive"
    )]
    ObservationSourceUnresolved {
        bank: String,
        index: usize,
        document: String,
        fact_index: i64,
    },

    /// `/documents.document_metadata` and `retain_params.metadata` were equal
    /// in all 25 live documents, and the import carries the second as the
    /// first. It is the one field with no `schema.py` counterpart, so
    /// `deny_unknown_fields` structurally cannot see it and only this
    /// comparison can.
    #[error(
        "{bank}: document {document} metadata disagrees — archive {archive}, /documents {legacy}"
    )]
    DocumentMetadataMismatch {
        bank: String,
        document: String,
        archive: String,
        legacy: String,
    },

    /// A `"bank"` archive carries mental models, directives and webhooks in
    /// files `load_dir` does not read (`schema.py:149`). Importing one would
    /// move the documents and leave the rest behind without a word.
    #[error(
        "{bank}: archive_type is {archive_type}, and only a documents archive is readable here"
    )]
    UnsupportedArchiveType { bank: String, archive_type: String },

    /// The plan puts mental models out of scope with *"there is nothing to
    /// migrate"* — measured 0 in every manifest, and asserted here because
    /// `--replace` **deletes** the target bank's `mental_models`
    /// (§Binding decisions #5d). A legacy bank that grew one would have it
    /// dropped on this side and not carried from that one.
    #[error("{bank}: manifest.{field} is {count}, and this importer carries none of them")]
    UnsupportedArchiveContent {
        bank: String,
        field: &'static str,
        count: i64,
    },

    /// The mirror of [`MigrateError::ObservationScopesUnsupported`] on the
    /// *fact* side, which D1's deferred list records as unchecked. Same silent
    /// drop, same reason `deny_unknown_fields` cannot catch it: the field is
    /// known and merely unused. Measured null in all 3,541 facts.
    #[error(
        "{bank}: document {document} fact #{fact_index} has observation_scopes {scopes}, \
         which we cannot store"
    )]
    FactScopesUnsupported {
        bank: String,
        document: String,
        fact_index: usize,
        scopes: String,
    },

    /// `banks.json` is the only carrier of a bank's mission and disposition —
    /// the transfer archive has neither. The mirror of
    /// [`MigrateError::StatsMissing`] for the other frozen file, and it is a
    /// refusal because the alternative is a bank created with a NULL mission
    /// and nothing saying the string was lost. `codex`'s hand-written
    /// 149-character mission is the reason `banks.json` exists at all.
    #[error("{bank}: an archive was loaded, but banks.json has no entry for it")]
    BankNotListed { bank: String },

    /// `occurred_start.or(mentioned_at)` came out `None`, so the node would
    /// land with no `event_date` — skipped by `temporal_links`
    /// (`links.rs:62-68`) — while legacy's own `event_date`, which is NOT NULL
    /// on its side precisely as this case's fallback (`schema.py:57-58`), is
    /// discarded. Measured 0 of 3,541 facts and 0 of 1,747 observations, and
    /// asserted for the same reason `original_text`'s 25/25 is.
    #[error(
        "{bank}: {document} #{index} has neither occurred_start nor mentioned_at, so event_date cannot be derived"
    )]
    EventDateNotDerivable {
        bank: String,
        document: String,
        index: usize,
    },

    /// Legacy types `fact_type` as a free string. Ours is a three-value
    /// `CHECK` (`0001_init.sql:33`), and defaulting an unrecognised value to
    /// `world` would file a shape change as ordinary content.
    #[error("{bank}: fact_type {fact_type:?} is not one of world/experience/observation")]
    UnknownFactType { bank: String, fact_type: String },

    /// A timestamp legacy emitted that we cannot read. Dropping it would cost
    /// the node its place in the temporal graph and say nothing.
    #[error("{bank}: {field} {value:?} is not an RFC 3339 timestamp")]
    BadTimestamp {
        bank: String,
        field: &'static str,
        value: String,
    },

    #[error("{bank}: embedding the observations failed: {message}")]
    Embed { bank: String, message: String },

    /// `drain_once` returns on the first embedder error with no retry
    /// (`embed_task.rs:110-124`), and its "model still loading" return
    /// (`:80-82`) is indistinguishable from a drained backlog out here. So the
    /// drain is bounded and the backlog must shrink between calls.
    #[error("{bank}: {pending} nodes still unembedded after {calls} drain call(s)")]
    EmbeddingBacklogStalled {
        bank: String,
        pending: i64,
        calls: usize,
    },

    /// Anything the store refused. Kept as a message rather than wrapping
    /// `memgarden_core::Error`: this module's whole error surface is one enum
    /// the binary prints with a cause walk, and the store's own `Display`
    /// already names the table and the constraint.
    #[error("{message}")]
    Store { message: String },
}

pub type Result<T> = std::result::Result<T, MigrateError>;

/// Reads a snapshot's `stats.json` — the frozen `/stats`, `/documents` and
/// `/memories/list` oracle, keyed by bank id.
pub fn load_stats(
    dir: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, snapshot::Stats>> {
    archive::read_json(&dir.join("stats.json"))
}

impl MigrateError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        MigrateError::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn json(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        MigrateError::Json {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! The two fixtures, and the distinction between them is the point.
    //!
    //! * **`real/`** — a redacted slice of the live
    //!   `claude-code::bank-a` archive: facts 125..210 re-indexed from
    //!   0, the observations whose sources all land inside that window, the
    //!   real `exported_at`, the real tag and metadata shapes. It carries what
    //!   a generator would not invent — a tag list mixing `file:` tags with a
    //!   bare document uuid, causal relations pointing both forward and
    //!   backward within one document, `occurred_start`/`occurred_end` null
    //!   with `mentioned_at` set.
    //! * **`edge/`** — hand-written and labelled synthetic, for shapes that
    //!   are *legal per `schema.py` but absent from today's corpus*.
    //!
    //! Conflating them is how the plan's first draft ended up specifying a
    //! fact-level `document_id` and a `context: ""` that do not exist in any
    //! of the 3,540 live facts.

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::archive::{BankArchive, load_dir};
    use super::snapshot::Stats;
    use super::{Result, load_stats};

    pub fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/migrate")
            .join(name)
    }

    pub fn real_fixture() -> PathBuf {
        fixture("real")
    }

    pub fn real_stats() -> Stats {
        load_stats(&real_fixture())
            .unwrap()
            .remove("claude-code::bank-a")
            .expect("the fixture's only bank")
    }

    /// One `edge/` bank, with the `Stats` recorded for it.
    pub fn edge_archive(bank_id: &str) -> (BankArchive, Stats) {
        let dir = fixture("edge");
        let archive = load_dir(&dir)
            .unwrap()
            .into_iter()
            .find(|a| a.bank_id == bank_id)
            .unwrap_or_else(|| panic!("no {bank_id} in the edge fixture"));
        let stats = load_stats(&dir).unwrap().remove(bank_id).unwrap();
        (archive, stats)
    }

    /// A writable snapshot directory assembled from one or more committed
    /// fixtures, with `stats.json` merged and `SHA256SUMS` **regenerated**.
    ///
    /// [`MutableSnapshot`]'s sibling, and separate from it on purpose: that
    /// one exists to break `assert_integrity`, which reads neither checksums
    /// nor `banks.json`. `import::run` reads both and verifies the checksums
    /// *before* it writes, so a mutation test that edits a file and forgets to
    /// reseal would fail on the checksum instead of on the property under
    /// test.
    ///
    /// It is also how multi-bank driving is covered without committing a third
    /// fixture that is just the other two side by side.
    pub struct Snapshot {
        dir: tempfile::TempDir,
    }

    impl Snapshot {
        /// `real/` — the redacted `claude-code::bank-a` slice.
        pub fn real() -> Snapshot {
            Snapshot::of(&["real"])
        }

        /// `real-dup/` — the redacted `claude-code::bank-b`
        /// slice, and the only fixture that can carry duplicate
        /// `(document_id, fact_index)` source pairs: all 86 in the live corpus
        /// are in that bank.
        pub fn real_cms() -> Snapshot {
            Snapshot::of(&["real-dup"])
        }

        /// Both real banks in one snapshot directory.
        pub fn both() -> Snapshot {
            Snapshot::of(&["real", "real-dup"])
        }

        /// One bank out of the three-bank `edge/` fixture, which has neither a
        /// `banks.json` nor a `SHA256SUMS` of its own and whose other two
        /// banks fail `assert_integrity` by design.
        pub fn edge(slug: &str, bank_id: &str) -> Snapshot {
            let scratch = tempfile::tempdir().unwrap();
            copy_dir(&fixture("edge").join(slug), &scratch.path().join(slug));
            let mut stats: serde_json::Value =
                serde_json::from_slice(&std::fs::read(fixture("edge").join("stats.json")).unwrap())
                    .unwrap();
            let entry = stats[bank_id].take();
            std::fs::write(
                scratch.path().join("stats.json"),
                serde_json::to_vec_pretty(&serde_json::json!({ bank_id: entry })).unwrap(),
            )
            .unwrap();
            std::fs::write(
                scratch.path().join("banks.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"banks": [{"bank_id": bank_id, "mission": null}]}),
                )
                .unwrap(),
            )
            .unwrap();
            let snapshot = Snapshot { dir: scratch };
            snapshot.reseal();
            snapshot
        }

        fn of(fixtures: &[&str]) -> Snapshot {
            let scratch = tempfile::tempdir().unwrap();
            let mut stats = serde_json::Map::new();
            let mut banks: Vec<serde_json::Value> = Vec::new();
            for name in fixtures {
                let from = fixture(name);
                for entry in std::fs::read_dir(&from).unwrap() {
                    let path = entry.unwrap().path();
                    let file = path.file_name().unwrap();
                    // Each fixture's own README, stats and checksums are per
                    // fixture; the bank directories are what compose.
                    if matches!(
                        file.to_str().unwrap(),
                        "README.md" | "stats.json" | "banks.json" | "SHA256SUMS"
                    ) {
                        continue;
                    }
                    copy_dir(&path, &scratch.path().join(file));
                }
                let read = |file: &str| -> serde_json::Value {
                    serde_json::from_slice(&std::fs::read(from.join(file)).unwrap()).unwrap()
                };
                for (bank, value) in read("stats.json").as_object().unwrap() {
                    stats.insert(bank.clone(), value.clone());
                }
                banks.extend(
                    read("banks.json")["banks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .cloned(),
                );
            }
            std::fs::write(
                scratch.path().join("stats.json"),
                serde_json::to_vec_pretty(&serde_json::Value::Object(stats)).unwrap(),
            )
            .unwrap();
            std::fs::write(
                scratch.path().join("banks.json"),
                serde_json::to_vec_pretty(&serde_json::json!({ "banks": banks })).unwrap(),
            )
            .unwrap();
            let snapshot = Snapshot { dir: scratch };
            snapshot.reseal();
            snapshot
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Edits one JSON file and reseals, so the mutation reaches the check
        /// under test rather than the checksum guard in front of it.
        pub fn edit(&self, relative: &str, f: impl FnOnce(&mut serde_json::Value)) {
            let path = self.dir.path().join(relative);
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            f(&mut value);
            std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            self.reseal();
        }

        pub fn reseal(&self) {
            super::snapshot::write_sha256sums(self.dir.path()).unwrap();
        }
    }

    /// A writable copy of `real/`. Mutation tests never touch the committed
    /// bytes — a test that edits its own fixture in place passes once and then
    /// lies.
    pub fn real_scratch() -> tempfile::TempDir {
        let scratch = tempfile::tempdir().unwrap();
        copy_dir(&real_fixture(), scratch.path());
        scratch
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy_dir(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }

    /// A scratch copy of `real/` with one field broken, so each integrity
    /// assertion can be shown to fail on a fixture that breaks *exactly* it.
    pub struct MutableSnapshot {
        scratch: tempfile::TempDir,
    }

    impl MutableSnapshot {
        pub fn real() -> Self {
            MutableSnapshot {
                scratch: real_scratch(),
            }
        }

        pub fn dir(&self) -> &Path {
            self.scratch.path()
        }

        pub fn edit_manifest(&mut self, f: impl FnOnce(&mut serde_json::Value)) {
            self.edit("claude-code__bank-a/manifest.json", f);
        }

        pub fn edit_document(&mut self, f: impl FnOnce(&mut serde_json::Value)) {
            self.edit("claude-code__bank-a/documents/000000.json", f);
        }

        pub fn edit_observations(&mut self, f: impl FnOnce(&mut serde_json::Value)) {
            self.edit("claude-code__bank-a/observations.json", f);
        }

        /// Edits the bank's entry in `stats.json`, not the whole file.
        pub fn edit_stats(&mut self, f: impl FnOnce(&mut serde_json::Value)) {
            self.edit("stats.json", |v| f(&mut v["claude-code::bank-a"]));
        }

        fn edit(&mut self, relative: &str, f: impl FnOnce(&mut serde_json::Value)) {
            let path = self.scratch.path().join(relative);
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            f(&mut value);
            std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }

        /// Reloads from disk and runs the check, so the mutation goes through
        /// the same parse path a real snapshot would.
        pub fn assert_integrity(&self) -> Result<()> {
            let archives = load_dir(self.dir())?;
            let stats: BTreeMap<String, Stats> = load_stats(self.dir())?;
            let archive = &archives[0];
            let bank_stats =
                stats
                    .get(&archive.bank_id)
                    .ok_or_else(|| super::MigrateError::StatsMissing {
                        bank: archive.bank_id.clone(),
                    })?;
            super::snapshot::assert_integrity(archive, bank_stats)
        }
    }
}
