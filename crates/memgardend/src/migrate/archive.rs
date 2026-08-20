//! The legacy transfer archive, as `serde` types.
//!
//! A field-for-field mirror of
//! `hindsight-api-slim/hindsight_api/engine/transfer/schema.py` — **including
//! the fields we will never use** (`chunks`, `original_text`,
//! `observation_scopes`, `consolidated_at`, the whole-bank manifest counters).
//! Representing them is what lets [`deny_unknown_fields`] mean something: a
//! type that models half the archive rejects the other half as "unknown" and
//! the strictness becomes noise.
//!
//! ```text
//! manifest.json              TransferManifest
//! documents/000000.json      TransferDocument   (one file per document)
//! observations.json          [TransferObservation]   (include_observations=true)
//! ```
//!
//! # Why every struct denies unknown fields
//!
//! Legacy's own importer is permissive — it model-validates and ignores what
//! it does not know (`engine/transfer/importer.py:122-123`). We are a one-way
//! consumer of a corpus that exists once, so the trade runs the other way: a
//! field legacy adds in an upgrade and we silently ignore is exactly the
//! "silent partial success" Phase D is built against. The strictness costs a
//! refusal we can read; permissiveness costs memory we cannot get back.
//!
//! `TransferDocument` gets it too, and that is deliberate rather than
//! incidental: it is the struct a legacy upgrade would most plausibly grow
//! (document-level metadata, a summary, a source pointer), and the first draft
//! of the plan scoped the attribute to facts and observations only.
//!
//! [`deny_unknown_fields`]: https://serde.rs/container-attrs.html#deny_unknown_fields

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{MigrateError, Result};

/// `schema.py:23`. Bumped by legacy when the archive layout changes in a
/// backward-incompatible way; [`super::snapshot::assert_integrity`] refuses
/// anything else.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 1;

/// `schema.py:31` — `Literal["per_tag", "combined", "all_combinations",
/// "shared"] | list[list[str]]`.
///
/// Modelled exactly rather than as a bare `Value` so the four legal literals
/// are written down in our tree. Every one of the 1,747 live observations has
/// this null; a non-null value is a refusal (there is no MemGarden column for
/// it), so this type exists to *name* what we are refusing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ObservationScopes {
    Named(NamedScope),
    Combinations(Vec<Vec<String>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedScope {
    PerTag,
    Combined,
    AllCombinations,
    Shared,
}

/// `schema.py:32`. Absent on v1 archives; legacy treats absent as `decoded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BankRowsJsonEncoding {
    Decoded,
    Serialized,
}

/// `schema.py:149`. `"documents"` in all four live banks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveType {
    Documents,
    Bank,
}

/// `schema.py:35-43`. `target_fact_index` is an ordinal into the **same
/// document's** `facts` list, not a database id, which is what makes causal
/// relations survive transfer at all.
///
/// Typed `i64` rather than `usize` because legacy types it `int`: a negative
/// or out-of-range value must parse and then be *rejected by name* (D2's
/// pre-write range check), not fail as a malformed number with no bank
/// attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferCausalRelation {
    pub relation_type: String,
    pub target_fact_index: i64,
}

/// `schema.py:46-79`. One extracted fact, without its embedding and without
/// its database id — both by design (`export.py:171-193`).
///
/// **There is no `document_id` field.** The archive's grouping *is* the
/// document: a fact's identity is `(document uuid, its ordinal in this
/// `facts` array)`, which is also the key legacy's own observation provenance
/// uses (`export.py:459-462`) and the only candidate measured unique across
/// all 3,540 facts.
///
/// Timestamps stay `String`. D1 converts nothing — it reads legacy, writes
/// files and asserts — and a parse here would turn a formatting surprise into
/// a snapshot failure rather than an import failure, which is the wrong PR to
/// discover it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferFact {
    pub text: String,
    pub fact_type: String,
    #[serde(default)]
    pub context: Option<String>,
    /// Legacy's fallback, used only when `occurred_start` and `mentioned_at`
    /// are both absent, to satisfy its NOT NULL column (`schema.py:57-58`).
    /// Ours is derived instead — `occurred_start.or(mentioned_at)`,
    /// `writes.py:80` parity — so this is carried, not consumed.
    #[serde(default)]
    pub event_date: Option<String>,
    #[serde(default)]
    pub occurred_start: Option<String>,
    #[serde(default)]
    pub occurred_end: Option<String>,
    #[serde(default)]
    pub mentioned_at: Option<String>,
    /// `dict[str, str]` in legacy, and string-valued in all 3,540 live facts.
    /// Kept strict: a non-string value is a shape change we want to hear about.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub observation_scopes: Option<ObservationScopes>,
    #[serde(default)]
    pub chunk_index: Option<i64>,
    /// Entity canonical names. Re-resolved against our bank by name
    /// (`graph::write_entities`), never by id — legacy exports no ids.
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub causal_relations: Vec<TransferCausalRelation>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The consolidation lifecycle, carried verbatim so an importer does not
    /// redo the work (`schema.py:71-79`). Our equivalent is a rowid watermark
    /// (`consolidate.rs:314-330`), so D2 collapses these 3,540 values into one
    /// `consolidation_runs` row per bank rather than a per-fact column.
    #[serde(default)]
    pub consolidated_at: Option<String>,
    #[serde(default)]
    pub consolidation_failed_at: Option<String>,
}

/// `schema.py:82-86`. Parsed and discarded: there is no chunk table
/// (`0001_init.sql:18-27`) and our retain re-derives chunking from the
/// transcript. Modelled anyway so `deny_unknown_fields` covers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferChunk {
    pub chunk_index: i64,
    pub chunk_text: String,
}

/// `schema.py:89-98`. An observation's source fact, by document + ordinal.
///
/// Duplicates within one observation are legal and real — 86 of them across
/// the live corpus, all in `claude-code::bank-b` — and they
/// collapse on import, because `link_sources_tx` is `INSERT OR IGNORE` against
/// the `(observation_id, source_id)` PK (`consolidate.rs:638-650`). That is
/// why D3's `node_sources` gate is 2,114 distinct pairs and not the 2,200 raw
/// ones.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferObservationSource {
    pub document_id: String,
    pub fact_index: i64,
}

/// `schema.py:101-118`. A consolidated observation — bank-level, not tied to
/// one document, carrying no embedding and no entity or link associations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferObservation {
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub event_date: Option<String>,
    #[serde(default)]
    pub occurred_start: Option<String>,
    #[serde(default)]
    pub occurred_end: Option<String>,
    #[serde(default)]
    pub mentioned_at: Option<String>,
    #[serde(default)]
    pub observation_scopes: Option<ObservationScopes>,
    /// Stored by legacy with a fallback (`proof_count or len(source_ids)`,
    /// `export.py:457`) where we always derive it from `node_sources`
    /// (`recount_proof_tx`, `consolidate.rs:658-666`). Measured: they differ
    /// in 93 of 1,747, in both directions. Carried for MG-2 to *report*, never
    /// to assert equal.
    #[serde(default = "one")]
    pub proof_count: i64,
    #[serde(default)]
    pub sources: Vec<TransferObservationSource>,
}

fn one() -> i64 {
    1
}

/// `schema.py:121-130`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferDocument {
    pub id: String,
    /// `str | None` in legacy (`schema.py:125`) and non-null in 24/24 today.
    /// The Option is kept because the refusal is only honest if the null is
    /// representable.
    #[serde(default)]
    pub original_text: Option<String>,
    #[serde(default)]
    pub retain_params: Option<serde_json::Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub chunks: Vec<TransferChunk>,
    #[serde(default)]
    pub facts: Vec<TransferFact>,
}

/// `schema.py:133-158`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferManifest {
    /// Defaulted to 1 rather than 0, mirroring pydantic (`schema.py:141`):
    /// an absent version legally *means* 1, and inventing a sentinel here
    /// would refuse an archive legacy considers well-formed.
    #[serde(default = "default_schema_version")]
    pub schema_version: i64,
    pub source_bank_id: String,
    #[serde(default)]
    pub exported_at: Option<String>,
    #[serde(default)]
    pub document_count: i64,
    #[serde(default)]
    pub fact_count: i64,
    #[serde(default)]
    pub observation_count: i64,
    #[serde(default = "default_archive_type")]
    pub archive_type: ArchiveType,
    #[serde(default)]
    pub mental_model_count: i64,
    #[serde(default)]
    pub directive_count: i64,
    #[serde(default)]
    pub webhook_count: i64,
    /// Added by a legacy upgrade after the AC-3 snapshot was ratified — the
    /// live daemon writes it, the archive this crate was built against did
    /// not. Modelled rather than waved through: `deny_unknown_fields` is the
    /// module's whole defence against a legacy release growing content we
    /// would silently not carry, and loosening it for one field spends that
    /// defence for every field after it. Asserted zero at import beside its
    /// siblings.
    #[serde(default)]
    pub knowledge_page_count: i64,
    #[serde(default)]
    pub includes_history: bool,
    #[serde(default)]
    pub bank_rows_json_encoding: Option<BankRowsJsonEncoding>,
}

fn default_schema_version() -> i64 {
    SUPPORTED_SCHEMA_VERSION
}

fn default_archive_type() -> ArchiveType {
    ArchiveType::Documents
}

/// One bank's unpacked archive, plus where it came from.
///
/// `bank_id` is read from `manifest.source_bank_id` and never from the
/// directory name: the directory name is a filesystem-safe slug of the bank id
/// (`::` and spaces do not survive), so the manifest is the only lossless
/// carrier of the real id.
#[derive(Debug, Clone)]
pub struct BankArchive {
    pub bank_id: String,
    pub dir: PathBuf,
    pub manifest: TransferManifest,
    pub documents: Vec<TransferDocument>,
    pub observations: Vec<TransferObservation>,
}

impl BankArchive {
    /// Every `causal_relations` entry across every document, paired with the
    /// index of the fact that authored it. The count MG-1 reconciles against
    /// `/stats.links_by_link_type.caused_by`, and — in D2 — the only link type
    /// copied rather than re-derived.
    ///
    /// Yields no document handle: D1 only counts and checks direction, and D2
    /// needs the resolved node ids rather than the archive rows anyway.
    pub fn causal_relations(&self) -> impl Iterator<Item = (usize, &TransferCausalRelation)> {
        self.documents.iter().flat_map(|d| {
            d.facts
                .iter()
                .enumerate()
                .flat_map(|(i, f)| f.causal_relations.iter().map(move |c| (i, c)))
        })
    }

    pub fn fact_count(&self) -> usize {
        self.documents.iter().map(|d| d.facts.len()).sum()
    }
}

/// Loads every bank archive under a snapshot directory, in slug order.
///
/// A bank archive is any immediate subdirectory holding a `manifest.json`;
/// `banks.json`, `stats.json`, `SHA256SUMS` and the `.zip` files sit beside
/// them and are ignored here. Sorted so a multi-bank run reports and fails in
/// the same order every time.
pub fn load_dir(dir: &Path) -> Result<Vec<BankArchive>> {
    let mut bank_dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| MigrateError::io(dir, e))? {
        let entry = entry.map_err(|e| MigrateError::io(dir, e))?;
        let path = entry.path();
        if path.is_dir() && path.join("manifest.json").is_file() {
            bank_dirs.push(path);
        }
    }
    if bank_dirs.is_empty() {
        return Err(MigrateError::NoArchives {
            dir: dir.to_path_buf(),
        });
    }
    bank_dirs.sort();
    bank_dirs.iter().map(|d| load_bank_dir(d)).collect()
}

/// Loads one unpacked bank archive.
///
/// `observations.json` is absent from a `include_observations=false` export
/// and is read as empty here rather than as an error — the count assertion in
/// [`super::snapshot::assert_integrity`] is what turns a missing observations
/// file into a refusal, and it names the shortfall instead of the file.
pub fn load_bank_dir(dir: &Path) -> Result<BankArchive> {
    let manifest: TransferManifest = read_json(&dir.join("manifest.json"))?;

    let documents_dir = dir.join("documents");
    let mut document_files: Vec<PathBuf> = Vec::new();
    if documents_dir.is_dir() {
        for entry in
            std::fs::read_dir(&documents_dir).map_err(|e| MigrateError::io(&documents_dir, e))?
        {
            let path = entry
                .map_err(|e| MigrateError::io(&documents_dir, e))?
                .path();
            if path.extension().is_some_and(|e| e == "json") {
                document_files.push(path);
            }
        }
    }
    // Zero-padded ordinals (`schema.py:6-12`), so a lexicographic sort is the
    // export order.
    document_files.sort();
    let documents = document_files
        .iter()
        .map(|p| read_json(p))
        .collect::<Result<Vec<TransferDocument>>>()?;

    let observations_path = dir.join("observations.json");
    let observations = if observations_path.is_file() {
        read_json(&observations_path)?
    } else {
        Vec::new()
    };

    Ok(BankArchive {
        bank_id: manifest.source_bank_id.clone(),
        dir: dir.to_path_buf(),
        manifest,
        documents,
        observations,
    })
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|e| MigrateError::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| MigrateError::json(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::test_support::real_fixture;

    /// The committed `real/` fixture is a redacted slice of the live
    /// `claude-code::bank-a` archive, and these are the shapes it
    /// exists to carry — none of which a hand-written fixture would have
    /// invented.
    #[test]
    fn the_real_fixture_carries_the_shapes_a_generator_would_not_invent() {
        let archives = load_dir(&real_fixture()).expect("fixture loads");
        assert_eq!(archives.len(), 1);
        let a = &archives[0];
        assert_eq!(a.bank_id, "claude-code::bank-a");
        let doc = &a.documents[0];

        // A tag list mixing `file:` tags with a bare document uuid.
        assert!(doc.tags.iter().any(|t| t.starts_with("file:")));
        assert!(doc.tags.contains(&doc.id));

        // Causal relations pointing both forward and backward within one
        // document — a generator writes only forward edges.
        let (mut forward, mut backward) = (0, 0);
        for (from, c) in a.causal_relations() {
            match c.target_fact_index.cmp(&(from as i64)) {
                std::cmp::Ordering::Greater => forward += 1,
                std::cmp::Ordering::Less => backward += 1,
                std::cmp::Ordering::Equal => panic!("self-causal fact {from}"),
            }
        }
        assert_eq!((forward, backward), (2, 2));

        // `occurred_start`/`occurred_end` null with `mentioned_at` set: the
        // majority shape, and the one that makes `event_date =
        // occurred_start.or(mentioned_at)` fall through to `mentioned_at`.
        let fallthrough = doc
            .facts
            .iter()
            .filter(|f| {
                f.occurred_start.is_none() && f.occurred_end.is_none() && f.mentioned_at.is_some()
            })
            .count();
        assert_eq!(fallthrough, 78, "78 of the 86 sliced facts");
        assert!(doc.facts.iter().any(|f| f.occurred_start.is_some()));

        // No fact carries a `document_id`: the grouping is the document.
        // Asserted structurally by `deny_unknown_fields` — see
        // `a_fact_with_a_document_id_field_is_rejected`.
        assert_eq!(a.fact_count(), 86);
        assert_eq!(a.observations.len(), 79);
    }

    /// `context` is never `""` or null in any of the 3,540 live facts, so the
    /// empty-string case is `edge/`'s job and not `real/`'s.
    #[test]
    fn context_is_populated_throughout_the_real_slice() {
        let archives = load_dir(&real_fixture()).unwrap();
        assert!(
            archives[0].documents[0]
                .facts
                .iter()
                .all(|f| f.context.as_deref().is_some_and(|c| !c.is_empty()))
        );
    }

    #[test]
    fn the_archive_round_trips_through_serde() {
        let archives = load_dir(&real_fixture()).unwrap();
        let doc = &archives[0].documents[0];
        let round_tripped: TransferDocument =
            serde_json::from_str(&serde_json::to_string(doc).unwrap()).unwrap();
        assert_eq!(&round_tripped, doc);

        let manifest = &archives[0].manifest;
        let round_tripped: TransferManifest =
            serde_json::from_str(&serde_json::to_string(manifest).unwrap()).unwrap();
        assert_eq!(&round_tripped, manifest);

        let obs = &archives[0].observations;
        let round_tripped: Vec<TransferObservation> =
            serde_json::from_str(&serde_json::to_string(obs).unwrap()).unwrap();
        assert_eq!(&round_tripped, obs);
    }

    /// The reason `deny_unknown_fields` is on every struct: a legacy upgrade
    /// that adds a field must stop the run, not be ignored.
    #[test]
    fn an_unknown_field_is_rejected_on_every_archive_struct() {
        let cases: [(&str, &str); 7] = [
            ("manifest", r#"{"source_bank_id":"b","surprise":1}"#),
            ("document", r#"{"id":"d","surprise":1}"#),
            ("fact", r#"{"text":"t","fact_type":"world","surprise":1}"#),
            (
                "chunk",
                r#"{"chunk_index":0,"chunk_text":"c","surprise":1}"#,
            ),
            ("observation", r#"{"text":"t","surprise":1}"#),
            (
                "observation source",
                r#"{"document_id":"d","fact_index":0,"surprise":1}"#,
            ),
            (
                "causal relation",
                r#"{"relation_type":"caused_by","target_fact_index":0,"surprise":1}"#,
            ),
        ];
        let parsed: [bool; 7] = [
            serde_json::from_str::<TransferManifest>(cases[0].1).is_ok(),
            serde_json::from_str::<TransferDocument>(cases[1].1).is_ok(),
            serde_json::from_str::<TransferFact>(cases[2].1).is_ok(),
            serde_json::from_str::<TransferChunk>(cases[3].1).is_ok(),
            serde_json::from_str::<TransferObservation>(cases[4].1).is_ok(),
            serde_json::from_str::<TransferObservationSource>(cases[5].1).is_ok(),
            serde_json::from_str::<TransferCausalRelation>(cases[6].1).is_ok(),
        ];
        for ((name, _), ok) in cases.iter().zip(parsed) {
            assert!(!ok, "{name} accepted an unknown field");
        }
    }

    /// A fact carries no `document_id` — the plan's first draft demanded one
    /// in the fixture and it does not exist. This is the assertion that keeps
    /// it from being reintroduced.
    #[test]
    fn a_fact_with_a_document_id_field_is_rejected() {
        let with_doc_id = r#"{"text":"t","fact_type":"world","document_id":"d"}"#;
        assert!(serde_json::from_str::<TransferFact>(with_doc_id).is_err());
    }

    /// Every field defaults exactly as pydantic does, so an older archive that
    /// predates a field still parses — the mirror is of the *defaults* too,
    /// not just the names.
    #[test]
    fn optional_fields_default_the_way_pydantic_does() {
        let m: TransferManifest = serde_json::from_str(r#"{"source_bank_id":"b"}"#).unwrap();
        assert_eq!(m.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(m.archive_type, ArchiveType::Documents);
        assert_eq!(m.document_count, 0);
        assert!(m.bank_rows_json_encoding.is_none());

        let o: TransferObservation = serde_json::from_str(r#"{"text":"t"}"#).unwrap();
        assert_eq!(o.proof_count, 1, "schema.py:117");
        assert!(o.sources.is_empty());

        let f: TransferFact = serde_json::from_str(r#"{"text":"t","fact_type":"world"}"#).unwrap();
        assert!(f.metadata.is_empty() && f.tags.is_empty() && f.causal_relations.is_empty());
    }

    #[test]
    fn observation_scopes_models_both_legal_shapes() {
        let named: TransferObservation =
            serde_json::from_str(r#"{"text":"t","observation_scopes":"per_tag"}"#).unwrap();
        assert_eq!(
            named.observation_scopes,
            Some(ObservationScopes::Named(NamedScope::PerTag))
        );
        let combos: TransferObservation =
            serde_json::from_str(r#"{"text":"t","observation_scopes":[["a","b"]]}"#).unwrap();
        assert_eq!(
            combos.observation_scopes,
            Some(ObservationScopes::Combinations(vec![vec![
                "a".to_string(),
                "b".to_string()
            ]]))
        );
        let absent: TransferObservation =
            serde_json::from_str(r#"{"text":"t","observation_scopes":null}"#).unwrap();
        assert_eq!(absent.observation_scopes, None);
    }

    #[test]
    fn load_dir_refuses_a_directory_with_no_bank_archive() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_dir(tmp.path()),
            Err(MigrateError::NoArchives { .. })
        ));
    }
}
