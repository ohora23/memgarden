//! AX-2 — the recall-*quality* harness: recall@1/5/10, MRR and nDCG@10 over a
//! fixed corpus and a graded gold label set.
//!
//! Phase B made at least ten decisions that move recall quality (RRF k=60,
//! 100/arm over-fetch, the un-ported `min_semantic`/`min_keyword` floors, the
//! `recallTypes` default, three alphas, the dual budget, the graph-arm
//! formula, temporal proximity). Every one was justified by port fidelity or
//! latency; none by measured quality. This binary is the instrument that lets
//! any of them be revisited with evidence, and the baseline it produces is
//! what CE-11 reports a delta against.
//!
//! ```text
//! recall_bench import <corpus.jsonl> <db-path>
//! recall_bench bench  <db-path> <gold.jsonl> <corpus.jsonl> [results.jsonl] [rerank=<top_k>]
//! ```
//!
//! `rerank=<top_k>` (CE-11) is the only measurement knob this binary exposes,
//! and it exists because it is the thing under test. Everything else — `now`,
//! `limit`, `max_tokens`, the budget and the recall types — is pinned to the
//! AX-2 baseline's configuration, since a delta computed under a different
//! configuration is not a delta.
//!
//! **A binary, not an `#[ignore]`d test.** The existing latency benches are
//! tests because they seed their own synthetic bank and assert nothing but a
//! printed number. This one takes a corpus path, a label path and an output
//! path, and its results are a committed artifact — a test harness would have
//! to hard-code all three and have its output scraped back out of
//! `--nocapture`. It is a tool, so it is a tool.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use memgarden_core::config::Config;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::nodes::NewNodeWithTags;
use memgarden_store::{Db, banks, graph, nodes};
use memgardend::links::{self, TimedNode};
use memgardend::recall::{RecallParams, TagsMatch};
use memgardend::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The bank the harness corpus lands in. One bank, because recall is
/// per-bank in both systems and the Phase C hook only ever queries the
/// project's own — a multi-bank corpus would measure a configuration nothing
/// runs in.
const BANK_ID: &str = "gold";

/// `now` is **pinned**, not read from the clock. Two scores depend on it —
/// `scoring::recency` and the temporal arm's window resolution — so a
/// wall-clock `now` would make the baseline drift every day and make
/// "지난주"/"어제" resolve to an empty window once the corpus ages. This is
/// 2026-08-03T00:00:00Z: midnight UTC after the newest fact in
/// `gold/corpus.jsonl` (2026-08-02T17:44:19Z), so the relative expressions in
/// the temporal stratum land on the corpus's own last days.
const BENCH_NOW_MS: i64 = 1_785_715_200_000;

/// Depth measured. recall@1/@5/@10, MRR and nDCG@10 all read from this
/// window; anything the ranker put below it counts as not retrieved.
const K: usize = 10;

/// Depth *requested*, and therefore the depth of the candidate pool the
/// labels were drawn from — deliberately deeper than `K`, because a pool
/// only as deep as the measurement makes recall@10 tautologically 1.0 for
/// every labelled query.
///
/// 20 is not an arbitrary "deeper": it is `[recall] limit`'s production
/// default, and `recall::over_fetch` clamps at a 100 minimum, so 10 and 20
/// produce the *same* per-arm over-fetch and therefore the same ranking. The
/// pool gets ten more candidates for free, with no configuration divergence
/// between what is labelled and what is measured. Raising it past 20 would
/// widen the over-fetch and change the ranking under measurement.
const POOL_LIMIT: usize = 20;

// ---------------------------------------------------------------------------
// Corpus snapshot
// ---------------------------------------------------------------------------

/// One line of `gold/corpus.jsonl` — the legacy fact as exported. Field names
/// and shapes are the legacy daemon's, not ours; the mapping into `NewNode`
/// happens in `import`.
#[derive(Debug, Deserialize)]
struct LegacyFact {
    /// The legacy uuid. Preserved verbatim as the MemGarden node uuid — gold
    /// labels key on it, so a rebuild that minted fresh uuids would break
    /// every label.
    id: String,
    text: String,
    context: Option<String>,
    fact_type: String,
    date: Option<String>,
    mentioned_at: Option<String>,
    occurred_start: Option<String>,
    occurred_end: Option<String>,
    /// Comma-separated resolved entity names, legacy's own format.
    entities: Option<String>,
    proof_count: Option<i64>,
    #[serde(default)]
    tags: Vec<String>,
}

/// RFC3339 (what the legacy daemon emits) to epoch ms.
fn iso_ms(s: &Option<String>) -> Option<i64> {
    s.as_deref()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<jiff::Timestamp>().ok())
        .map(|ts| ts.as_millisecond())
}

fn read_corpus(path: &Path) -> anyhow::Result<Vec<LegacyFact>> {
    let raw = std::fs::read_to_string(path)?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<LegacyFact>(l).map_err(Into::into))
        .collect()
}

fn sha256_of(path: &Path) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path)?);
    Ok(format!("{:x}", hasher.finalize()))
}

/// The legacy fact's four timestamps and its type, resolved to what
/// `NewNode` wants. Kept as a struct rather than a tuple because three of the
/// five fields are `Option<i64>` and a positional swap between them would be
/// a silent temporal corruption, not a compile error.
#[derive(Debug, Clone, Copy)]
struct Draft {
    fact_type: FactType,
    event_date: Option<i64>,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
}

impl Draft {
    fn from_legacy(f: &LegacyFact) -> Draft {
        let occurred_start = iso_ms(&f.occurred_start);
        let occurred_end = iso_ms(&f.occurred_end);
        // Legacy leaves `mentioned_at` null on some older rows; its `date`
        // column is the same clock and is always set.
        let mentioned_at = iso_ms(&f.mentioned_at).or_else(|| iso_ms(&f.date));
        Draft {
            fact_type: f.fact_type.parse().unwrap_or(FactType::World),
            // `writes.py:80`, mirrored by `retain::NodeDraft::build`.
            event_date: occurred_start.or(mentioned_at),
            occurred_start,
            occurred_end,
            mentioned_at,
        }
    }
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// Builds the harness corpus: legacy fact text in, MemGarden's own vectors,
/// entities and links out.
///
/// **Text, not vectors, and no `/retain` round trip.** The plan's wording is
/// "re-retain", but running our qwen3-14b extraction over text that is
/// already an extracted fact would cost hours *and rewrite the text* — which
/// silently invalidates every gold label the moment it runs. Importing the
/// fact text directly and letting our own fastembed produce the vectors is
/// what makes AX-1's "two embedding spaces" problem disappear here, and it is
/// the only version that reproduces byte-identically from the snapshot.
///
/// The derived structures are built by the *production* code paths, not
/// reimplemented: `links::temporal_links`, `graph::write_entities`, and
/// `embed_task::drain_once` (which also runs the semantic-link pass). What
/// this corpus is missing relative to a real retain is the causal links —
/// legacy's export carries no `causal_relations` — and that is recorded in
/// the design note rather than invented here.
async fn import(corpus_path: &Path, db_path: &Path) -> anyhow::Result<()> {
    if db_path.exists() {
        anyhow::bail!(
            "{} already exists; an import must start from an empty database or it is not \
             reproducible",
            db_path.display()
        );
    }
    let facts = read_corpus(corpus_path)?;
    println!(
        "import: {} facts from {}",
        facts.len(),
        corpus_path.display()
    );

    let db = Arc::new(Db::open(db_path)?);
    banks::create(&db, BANK_ID, None, None)?;

    // --- Nodes ----------------------------------------------------------
    let drafts: Vec<Draft> = facts.iter().map(Draft::from_legacy).collect();

    let items: Vec<NewNodeWithTags> = facts
        .iter()
        .zip(&drafts)
        .map(|(f, d)| NewNodeWithTags {
            node: NewNode {
                bank_id: BANK_ID,
                document_id: None,
                fact_type: d.fact_type,
                text: &f.text,
                context: f.context.as_deref().filter(|c| !c.is_empty()),
                event_date: d.event_date,
                occurred_start: d.occurred_start,
                occurred_end: d.occurred_end,
                mentioned_at: d.mentioned_at,
                metadata: None,
            },
            tags: &f.tags,
        })
        .collect();
    let ids = nodes::insert_batch(&db, &items)?;
    drop(items);

    // Two columns `insert_batch` does not take, written in one pass:
    //   * `uuid` — `insert_batch` mints a fresh v7 per row. The gold labels
    //     key on the *legacy* uuid, so the import must overwrite it or the
    //     labels only match the one database that happened to be built first.
    //   * `proof_count` — a plain column (`0004`), recounted by a trigger on
    //     `node_sources` only. Carrying it preserves CE-9a's +5% proof boost
    //     for the 152 multi-source observations in the corpus.
    db.write(|tx| {
        for (id, f) in ids.iter().zip(&facts) {
            tx.execute(
                "UPDATE memory_nodes SET uuid = ?1, proof_count = ?2 WHERE id = ?3",
                rusqlite::params![f.id, f.proof_count.unwrap_or(0), id],
            )
            .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
        }
        Ok(())
    })?;
    println!("import: {} nodes written", ids.len());

    // --- Entities (the graph arm's co-membership signal) ------------------
    // `first_seen`/`last_seen` come from the fact's own date, which is what
    // `write_entities` documents and what `retain::write_graph` passes.
    let mentions: Vec<graph::EntityMentions> = ids
        .iter()
        .zip(&facts)
        .zip(&drafts)
        .filter_map(|((id, f), d)| {
            let names: Vec<String> = f
                .entities
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect();
            (!names.is_empty()).then(|| (*id, names, d.event_date.unwrap_or(BENCH_NOW_MS)))
        })
        .collect();
    let entities = graph::write_entities(&db, BANK_ID, &mentions, BENCH_NOW_MS)?;
    println!(
        "import: {} entity rows from {} facts",
        entities.len(),
        mentions.len()
    );

    // --- Temporal links ---------------------------------------------------
    // Retain calls this per chunk against a rolling window; the whole corpus
    // is passed as both sides here instead, which is a *different* edge set
    // and not merely a cheaper route to the same one.
    //
    // The claim that used to stand here — "converges to the same edge set (a
    // node's 20 best 24h-neighbours) without replaying the ingest order" —
    // was never measured and is wrong. MG-1 measured it three ways over the
    // legacy transfer archive: a whole-corpus rebuild gives 70,192 temporal
    // edges, a replay grouped by `chunk_index` gives 68,781, and a replay
    // grouped by the `created_at` batch boundary gives 69,771. Ordering moves
    // the result by 2 %, so it is not nothing — a rolling window caps each
    // node against the neighbours it had *at the time*, and the whole corpus
    // caps against every neighbour that ever existed.
    //
    // It stays whole-corpus here on purpose: this harness has to reproduce
    // byte-identically from the snapshot, and a replay would make the edge
    // set depend on a `chunk_index` the corpus does not carry. See
    // `docs/design/mg-1-migration.md` §"Temporal links are not legacy's".
    let timed: Vec<TimedNode> = ids
        .iter()
        .zip(&drafts)
        .filter_map(|(id, d)| {
            d.event_date.map(|event_date| TimedNode {
                id: *id,
                fact_type: d.fact_type.as_str().to_string(),
                event_date,
            })
        })
        .collect();
    let temporal = links::temporal_links(&timed, &timed);
    let written = graph::insert_links(&db, &temporal, BENCH_NOW_MS)?;
    println!("import: {written} temporal links");

    // --- Embeddings + semantic links --------------------------------------
    // The real backlog worker, not a copy of it: `drain_once` loops until a
    // partial batch comes back, so one call drains the whole corpus, and its
    // `on_batch_embedded` writes the semantic links exactly as production
    // does.
    let (state, _retain_rx) = build_state(db.clone(), Config::defaults()?)?;
    let started = std::time::Instant::now();
    load_embedder(&state).await?;
    memgardend::embed_task::drain_once(&db, &state).await;
    let pending = nodes::pending_embeddings(&db, 1)?.len();
    anyhow::ensure!(pending == 0, "embedding backlog did not drain");
    println!(
        "import: embeddings + semantic links in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Gold labels
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GoldQuery {
    id: String,
    /// `memcompare | identifier | conclusion | temporal | graph`. Aggregates
    /// are reported per stratum because that is the only axis on which
    /// "hybrid with no cosine floor protects proper-noun queries" is visible.
    stratum: String,
    query: String,
    /// **Per query, not per run.** `provisional-pending-user-review` until the
    /// corpus owner signs *this query's* labels off, then
    /// `ratified-YYYY-MM-DD`; `unlabelled` before the first pass. Carried
    /// through into every results record — both per-query and as the run-level
    /// union — so a number can never be quoted without the caveat travelling
    /// with it.
    ///
    /// The field was always per-query; ratification just made that load-bearing
    /// (q17 is signed off, the other 19 are not). The run-level
    /// `labels_status` is the sorted **set** of the values present, so a mixed
    /// run reports both and cannot read as fully ratified.
    labels_status: String,
    /// Free text; carries "no answer in this corpus" for an honestly empty
    /// query, which is data rather than a gap to be papered over with a
    /// guessed label.
    #[serde(default)]
    note: String,
    labels: Vec<GoldLabel>,
}

#[derive(Debug, Deserialize)]
struct GoldLabel {
    uuid: String,
    /// 2 = core (answers the query), 1 = related (useful context),
    /// 0 = judged and rejected. A 0 is kept rather than dropped: it records
    /// that a plausible-looking hit was *examined*, which is the difference
    /// between a labelled negative and an unlabelled one.
    grade: u8,
    /// One line of rationale, mandatory. Six months from now nobody can
    /// re-derive "why is this relevant", and an unexplained set gets thrown
    /// away rather than trusted.
    #[allow(
        dead_code,
        reason = "read by humans reviewing the file, not by the harness"
    )]
    why: String,
}

fn read_gold(path: &Path) -> anyhow::Result<Vec<GoldQuery>> {
    let raw = std::fs::read_to_string(path)?;
    let queries: Vec<GoldQuery> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str::<GoldQuery>)
        .collect::<Result<_, _>>()?;
    for q in &queries {
        for l in &q.labels {
            anyhow::ensure!(
                !l.why.trim().is_empty(),
                "{}: label {} has no rationale",
                q.id,
                l.uuid
            );
            anyhow::ensure!(l.grade <= 2, "{}: grade {} out of range", q.id, l.grade);
        }
    }
    Ok(queries)
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Graded nDCG, **exponential gain and binary-log discount**:
///
/// ```text
/// DCG@k = Σ_{i=1..k} (2^grade_i - 1) / log2(i + 1)
/// ```
///
/// so grade 2 → gain 3, grade 1 → gain 1, grade 0 → gain 0, and rank 1 is
/// undiscounted. This is the Burges/TREC formulation, not Järvelin-Kekäläinen's
/// original linear gain (`grade / log2(i+1)`), and not the "no discount on the
/// first two ranks" variant. Stating it matters: three conventions are in
/// common use and they do not produce the same number, so a future reader
/// comparing against a published figure has to know which one these are.
///
/// The ideal DCG sorts *all* labelled grades descending and truncates at `k`,
/// so a query with more than `k` relevant nodes can still reach 1.0.
fn ndcg_at_k(grades_in_rank_order: &[u8], all_grades: &[u8], k: usize) -> f64 {
    fn dcg(grades: impl Iterator<Item = u8>) -> f64 {
        grades
            .enumerate()
            .map(|(i, g)| (2f64.powi(i32::from(g)) - 1.0) / ((i + 2) as f64).log2())
            .sum()
    }
    let mut ideal: Vec<u8> = all_grades.to_vec();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg = dcg(ideal.into_iter().take(k));
    if idcg == 0.0 {
        return 0.0;
    }
    dcg(grades_in_rank_order.iter().copied().take(k)) / idcg
}

#[derive(Debug, Clone, Copy, Serialize)]
struct QueryMetrics {
    #[serde(rename = "recall@1")]
    recall_1: f64,
    #[serde(rename = "recall@5")]
    recall_5: f64,
    #[serde(rename = "recall@10")]
    recall_10: f64,
    mrr: f64,
    #[serde(rename = "ndcg@10")]
    ndcg_10: f64,
    /// `min(K, |relevant|) / |relevant|` — the largest recall@10 this query
    /// can reach. This corpus is heavy with near-duplicate facts (the same
    /// statement retained as both a world and an observation node, sometimes
    /// four times), so several queries have 15-18 relevant nodes and a
    /// recall@10 ceiling near 0.55. Reported alongside because a raw 0.33
    /// read as "out of 1.0" is a misreading, and nDCG (whose ideal list is
    /// also truncated at 10) is the metric that is *not* deflated this way.
    #[serde(rename = "recall@10_ceiling")]
    recall_10_ceiling: f64,
}

impl QueryMetrics {
    fn compute(retrieved: &[String], labels: &[GoldLabel]) -> QueryMetrics {
        let by_uuid: HashMap<&str, u8> =
            labels.iter().map(|l| (l.uuid.as_str(), l.grade)).collect();
        // "Relevant" is grade >= 1 for the set metrics; the 2/1 split only
        // shows up in nDCG, which is the point of grading at all.
        let relevant: HashSet<&str> = labels
            .iter()
            .filter(|l| l.grade >= 1)
            .map(|l| l.uuid.as_str())
            .collect();
        let graded: Vec<u8> = retrieved
            .iter()
            .map(|u| by_uuid.get(u.as_str()).copied().unwrap_or(0))
            .collect();

        let hits_at = |k: usize| {
            retrieved
                .iter()
                .take(k)
                .filter(|u| relevant.contains(u.as_str()))
                .count() as f64
        };
        // Denominator is the size of the *labelled* relevant set, so
        // recall@1 is capped by construction whenever a query has more than
        // one relevant node. That is the standard definition and the reason
        // recall@1 is read as a floor, not as precision@1.
        let denom = relevant.len() as f64;
        let rr = retrieved
            .iter()
            .position(|u| relevant.contains(u.as_str()))
            .map_or(0.0, |i| 1.0 / (i + 1) as f64);

        QueryMetrics {
            recall_1: hits_at(1) / denom,
            recall_5: hits_at(5) / denom,
            recall_10: hits_at(K.min(10)) / denom,
            mrr: rr,
            ndcg_10: ndcg_at_k(
                &graded,
                &labels.iter().map(|l| l.grade).collect::<Vec<_>>(),
                10,
            ),
            recall_10_ceiling: (K.min(relevant.len()) as f64) / denom,
        }
    }
}

/// Macro-average: every query counts once, whatever its number of relevant
/// nodes. A micro-average would let the two or three broad queries dominate
/// the aggregate and hide exactly the per-stratum weakness the stratification
/// exists to expose.
fn mean(rows: &[QueryMetrics]) -> QueryMetrics {
    // An all-unlabelled run (the first pass, used only to print the pool)
    // would otherwise report NaN across the board and look like a failure.
    let n = (rows.len() as f64).max(1.0);
    let sum = |f: fn(&QueryMetrics) -> f64| rows.iter().map(f).sum::<f64>() / n;
    QueryMetrics {
        recall_1: sum(|m| m.recall_1),
        recall_5: sum(|m| m.recall_5),
        recall_10: sum(|m| m.recall_10),
        mrr: sum(|m| m.mrr),
        ndcg_10: sum(|m| m.ndcg_10),
        recall_10_ceiling: sum(|m| m.recall_10_ceiling),
    }
}

// ---------------------------------------------------------------------------
// bench
// ---------------------------------------------------------------------------

/// `rerank_top_k`: `None` is the AX-2 baseline configuration (RRF passthrough,
/// which is production); `Some(k)` turns CE-11's cross-encoder on at that
/// depth. It is the *only* knob a CE-11 run may change — every other value
/// below is pinned to what the baseline used, because a delta computed under
/// a different configuration is not a delta.
///
/// `Some(k)` is close to inert on recall@10 but **not** pinned at zero: the
/// reranker truncates on *RRF* order, while the baseline's top 10 is ordered
/// by `passthrough_base x boosts`, so the +/-21% boost envelope can swap items
/// across the rank-10 boundary in either direction. Measured at `k = 10`:
/// overall recall@10 moved 0.3345 -> 0.3363, and the memcompare stratum
/// *dropped* 0.3476 -> 0.3276. Small, signed, real. MRR and nDCG@10 are still
/// the columns with something to say.
async fn bench(
    db_path: &Path,
    gold_path: &Path,
    corpus_path: &Path,
    out_path: Option<&Path>,
    rerank_top_k: Option<usize>,
) -> anyhow::Result<()> {
    let gold = read_gold(gold_path)?;
    let corpus_sha = sha256_of(corpus_path)?;
    let corpus_lines = read_corpus(corpus_path)?.len();

    let db = Arc::new(Db::open(db_path)?);
    let node_count = nodes::count(&db, BANK_ID)?;
    // The gold labels are only meaningful against the corpus they were
    // written for. This catches the "benched the wrong database" mistake,
    // which otherwise looks like a quality regression.
    anyhow::ensure!(
        node_count as usize == corpus_lines,
        "database has {node_count} nodes but {} has {corpus_lines} facts — rebuild with \
         `recall_bench import`",
        corpus_path.display()
    );

    let mut cfg = Config::defaults()?;
    if let Some(top_k) = rerank_top_k {
        cfg.reranker.enabled = true;
        cfg.reranker.top_k = top_k;
    }
    let limit = POOL_LIMIT;
    let budget = cfg.profile.recall_budget.clone();
    let (state, _retain_rx) = build_state(db.clone(), cfg)?;
    load_embedder(&state).await?;
    if rerank_top_k.is_some() {
        load_reranker(&state).await?;
    }

    let mut rows: Vec<(&GoldQuery, QueryMetrics, Vec<String>)> = Vec::new();
    let mut unanswered: Vec<&GoldQuery> = Vec::new();
    let mut pool = serde_json::Map::new();

    for q in &gold {
        let params = RecallParams {
            query: q.query.clone(),
            limit,
            budget: budget.clone(),
            // NOT the production 1024. The token budget truncates the result
            // list (`recall::recall`'s `fit_to_budget`), so leaving it at the
            // default would report nDCG@10 over a list the budget had already
            // cut to six — measuring the budget, not the ranking. The budget
            // is a real lever, but it is a *different* lever and CE-11 tunes
            // the ranker.
            max_tokens: memgarden_core::config::MAX_RECALL_TOKENS,
            fact_types: vec![FactType::World, FactType::Observation, FactType::Experience],
            tags: vec![],
            tags_match: TagsMatch::Any,
            cap_per_source: 0,
            preamble: String::new(),
            now_ms: BENCH_NOW_MS,
        };
        let outcome = memgardend::recall::recall(&state, BANK_ID.to_string(), params)
            .await
            .map_err(|e| anyhow::anyhow!("recall failed for {}: {e:?}", q.id))?;
        let mut retrieved: Vec<String> = outcome.results.iter().map(|r| r.uuid.clone()).collect();

        // Always dumped, labelled or not: this IS the candidate pool the
        // labels were drawn from, so it doubles as the audit trail for how
        // they were produced and as the input to the next labelling round.
        pool.insert(
            q.id.clone(),
            json!({
                "query": q.query,
                "results": outcome.results.iter().map(|r| json!({
                    "uuid": r.uuid,
                    "type": r.fact_type.as_str(),
                    "final": r.scores.final_score,
                    "semantic": r.scores.semantic,
                    "keyword": r.scores.keyword,
                    "text": r.text,
                })).collect::<Vec<_>>(),
            }),
        );

        if q.labels.iter().all(|l| l.grade == 0) {
            unanswered.push(q);
            continue;
        }
        // Metrics see only the top `K`; the pool above keeps all
        // `POOL_LIMIT`. A relevant node at rank 15 is a miss here, which is
        // what "@10" means.
        retrieved.truncate(K);
        rows.push((q, QueryMetrics::compute(&retrieved, &q.labels), retrieved));
    }

    // --- Report ----------------------------------------------------------
    let scored: Vec<QueryMetrics> = rows.iter().map(|(_, m, _)| *m).collect();
    let overall = mean(&scored);
    let mut strata: Vec<(String, QueryMetrics, usize)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (q, ..) in &rows {
        if seen.contains(&q.stratum.as_str()) {
            continue;
        }
        seen.push(&q.stratum);
        let members: Vec<QueryMetrics> = rows
            .iter()
            .filter(|(other, ..)| other.stratum == q.stratum)
            .map(|(_, m, _)| *m)
            .collect();
        strata.push((q.stratum.clone(), mean(&members), members.len()));
    }

    let line = |left: String, name: &str, m: &QueryMetrics, rel: String| {
        println!(
            "{left:<6} {name:<12} {:>5} {:>7.3} {:>7.3} {:>8.3} {:>8.3} {:>7.3} {:>8.3}",
            rel, m.recall_1, m.recall_5, m.recall_10, m.recall_10_ceiling, m.mrr, m.ndcg_10
        )
    };
    println!(
        "\ncorpus {corpus_sha} ({node_count} nodes), now = {BENCH_NOW_MS}, rerank = {}",
        rerank_top_k.map_or("off".to_string(), |k| format!("top_k {k}"))
    );
    println!(
        "{:<6} {:<12} {:>5} {:>7} {:>7} {:>8} {:>8} {:>7} {:>8}",
        "query", "stratum", "|R|", "r@1", "r@5", "r@10", "ceil", "mrr", "nDCG@10"
    );
    for (q, m, _) in &rows {
        let rel = q.labels.iter().filter(|l| l.grade >= 1).count();
        line(q.id.clone(), &q.stratum, m, rel.to_string());
    }
    println!();
    for (name, m, n) in &strata {
        line(format!("({n})"), name, m, "-".to_string());
    }
    line(
        format!("({})", rows.len()),
        "ALL",
        &overall,
        "-".to_string(),
    );
    for q in &unanswered {
        println!("unanswered: {} — {}", q.id, q.note);
    }

    let mut label_status: Vec<&str> = gold.iter().map(|q| q.labels_status.as_str()).collect();
    label_status.sort_unstable();
    label_status.dedup();
    // Printed, not only written: a run whose queries are a mix of ratified and
    // provisional must say so on the terminal the numbers were read off, or the
    // caveat only exists in a file nobody opens.
    println!("\nlabels_status: {}", label_status.join(", "));

    // The number this run must be read against, printed beside it. See
    // `compare_to_ledger`.
    let ledger_path = out_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| gold_path.with_file_name("results.jsonl"));
    compare_to_ledger(&ledger_path, &corpus_sha, rerank_top_k, &overall);

    let record = json!({
        "run_at_ms": memgarden_core::now_ms(),
        "commit": std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string()),
        "corpus": { "sha256": corpus_sha, "nodes": node_count },
        "config": {
            "limit": limit, "budget": budget, "k": K,
            "now_ms": BENCH_NOW_MS,
            "max_tokens": memgarden_core::config::MAX_RECALL_TOKENS,
            // The treatment. `null` is the AX-2 baseline (RRF passthrough).
            "rerank_top_k": rerank_top_k,
        },
        // The **set** of per-query statuses present in this run, sorted. Travels
        // with every number so a figure can never be quoted without the caveat
        // attached to it — and because it is a set rather than a single flag, a
        // run mixing ratified and provisional queries reports both values and
        // cannot be read as fully ratified. The per-query status is on each
        // `per_query` entry below, which is the resolution a per-query figure
        // needs.
        "labels_status": label_status,
        "scored_queries": rows.len(),
        "unanswered": unanswered.iter().map(|q| &q.id).collect::<Vec<_>>(),
        "overall": overall,
        "per_stratum": strata.iter()
            .map(|(name, m, n)| json!({ "stratum": name, "queries": n, "metrics": m }))
            .collect::<Vec<_>>(),
        "per_query": rows.iter()
            .map(|(q, m, retrieved)| json!({
                "id": q.id, "stratum": q.stratum, "metrics": m,
                "labels_status": q.labels_status,
                "relevant": q.labels.iter().filter(|l| l.grade >= 1).count(),
                "retrieved": retrieved,
            }))
            .collect::<Vec<_>>(),
    });
    if let Some(out) = out_path {
        use std::io::Write;
        // Appended, one JSON object per line: the file is a ledger CE-11 adds
        // to, so a rewrite would destroy the baseline it reports against.
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)?;
        writeln!(f, "{record}")?;
        // The pool is rewritten each run, unlike the ledger — so a reranked
        // run writes to its own file rather than overwriting the baseline's
        // labelling pool, which is the artifact the gold labels were drawn
        // from and the audit trail for how they were produced.
        let pool_path = match rerank_top_k {
            Some(k) => out.with_extension(format!("rerank{k}.pool.json")),
            None => out.with_extension("pool.json"),
        };
        std::fs::write(&pool_path, serde_json::to_string_pretty(&pool)?)?;
        println!(
            "\nappended to {}, pool in {}",
            out.display(),
            pool_path.display()
        );
    } else {
        println!("\n{}", serde_json::to_string_pretty(&record)?);
        println!("{}", serde_json::to_string_pretty(&pool)?);
    }
    Ok(())
}

/// The ledger row this run must be read against: the **last** line matching
/// both the corpus digest and `rerank_top_k`, with its 1-based line number so
/// a write-up can cite it.
///
/// Last, not best and not first — the ledger is append-only and a later row at
/// the same configuration supersedes an earlier one. Reading an earlier row as
/// the standing baseline is the exact mistake this guard was added for.
/// Unparseable lines are skipped rather than fatal, and they still consume a
/// line number, because the number has to match what an editor shows.
fn latest_matching(
    text: &str,
    corpus_sha: &str,
    rerank_top_k: Option<usize>,
) -> Option<(usize, serde_json::Value)> {
    let want = rerank_top_k.map_or(serde_json::Value::Null, |k| json!(k));
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| Some((i + 1, serde_json::from_str::<serde_json::Value>(l).ok()?)))
        .filter(|(_, v)| v["corpus"]["sha256"] == corpus_sha && v["config"]["rerank_top_k"] == want)
        .last()
}

/// Print the newest ledger entry taken under this run's corpus and
/// configuration, and the delta to it.
///
/// This exists because of a mistake that cost a full investigation. The
/// baseline was quoted from ledger line 8 (`recall@10 0.3881`) while lines 11
/// and 12 — the two most recent runs at the same configuration — both record
/// **0.3792**, the drop CE-7's semantic-link fix caused and that
/// `docs/design/mg-1-migration.md` explains. A fresh import reproduced 0.3792
/// exactly, and the harness was written up as non-deterministic on the
/// strength of a comparison against a superseded row.
///
/// A ledger is only a baseline if the run reads it. Two rules follow from how
/// the mistake happened:
///
/// * it reads the ledger even when `out_path` is `None` — that run writes
///   nothing, which is exactly when a stale number in a document is the only
///   thing left to compare against;
/// * it matches on corpus digest **and** `rerank_top_k`, because a CE-11 run
///   and a baseline run are not each other's baseline.
///
/// A missing or unparseable ledger is not an error: the first run against a
/// new corpus has nothing to compare to and should still print its numbers.
fn compare_to_ledger(
    path: &Path,
    corpus_sha: &str,
    rerank_top_k: Option<usize>,
    overall: &QueryMetrics,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!(
            "\nno ledger at {} — nothing to compare against",
            path.display()
        );
        return;
    };
    let Some((line_no, prev)) = latest_matching(&text, corpus_sha, rerank_top_k) else {
        println!("\nno ledger entry for this corpus at rerank_top_k={rerank_top_k:?} — first run");
        return;
    };

    let commit = prev["commit"].as_str().unwrap_or("?");
    println!(
        "\nbaseline: {} line {} ({})",
        path.display(),
        line_no,
        // `char_indices`, not a byte slice: a hand-edited or replayed ledger
        // can carry a non-hex `commit`, and cutting mid-character panics.
        commit
            .char_indices()
            .nth(8)
            .map_or(commit, |(byte, _)| &commit[..byte])
    );
    let mut compared = 0usize;
    let mut identical = true;
    for (name, now, was) in [
        ("recall@5", overall.recall_5, &prev["overall"]["recall@5"]),
        (
            "recall@10",
            overall.recall_10,
            &prev["overall"]["recall@10"],
        ),
        ("mrr", overall.mrr, &prev["overall"]["mrr"]),
        ("nDCG@10", overall.ndcg_10, &prev["overall"]["ndcg@10"]),
    ] {
        let Some(was) = was.as_f64() else { continue };
        // Bit equality, not a tolerance. The harness is deterministic — three
        // imports of the frozen corpus produced hash-identical nodes, links,
        // entities and vectors — so any difference at all is a change in
        // behaviour, and a tolerance here would hide exactly the small signed
        // moves this benchmark exists to measure.
        let same = was.to_bits() == now.to_bits();
        compared += 1;
        identical &= same;
        println!(
            "  {name:<10} {was:.16} -> {now:.16}  {}",
            if same {
                "same".to_string()
            } else {
                format!("{:+.4}", now - was)
            }
        );
    }
    // `compared > 0` matters: a row carrying no recognisable aggregate would
    // otherwise vacuously "reproduce", which is the failure mode this whole
    // guard exists to prevent, one level down.
    if identical && compared == 4 {
        println!("  reproduces line {line_no} to the digit");
    } else if identical {
        // Every aggregate the row *had* matched, but it did not have all four.
        // "reproduces to the digit" would overclaim on a partial row, which is
        // the same mistake as the one this guard exists to prevent.
        println!(
            "  line {line_no} carries only {compared} of the 4 aggregates — partial match, not a baseline"
        );
    }
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// An `AppState` with no daemon around it. `recall::recall` and
/// `embed_task::drain_once` take one, and building it by hand is cheaper than
/// standing up an HTTP server to talk to over loopback.
///
/// The receiver is returned rather than dropped: dropping it closes the
/// channel, and a closed `retain_tx` is a footgun for anything added here
/// later. Nothing in this binary retains.
fn build_state(
    db: Arc<Db>,
    cfg: Config,
) -> anyhow::Result<(
    AppState,
    tokio::sync::mpsc::Receiver<memgardend::retain::RetainTask>,
)> {
    let mut cfg = cfg;
    // Loopback port 1: nothing in this binary calls Ollama, and a
    // misconfiguration should fail fast rather than quietly hit a real model.
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.request_timeout_secs = 1;
    cfg.ollama.max_retries = 0;
    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone())?);
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    Ok((
        AppState {
            db,
            cfg: Arc::new(cfg),
            started_at_ms: memgarden_core::now_ms(),
            embedder: Arc::new(RwLock::new(None)),
            reranker: Default::default(),
            ollama,
            consolidating: Default::default(),
            refreshing: Default::default(),
            retain_tx,
            events: memgardend::events::channel(),
        },
        retain_rx,
    ))
}

async fn load_embedder(state: &AppState) -> anyhow::Result<()> {
    let cfg = state.cfg.embedding.clone();
    let embedder =
        tokio::task::spawn_blocking(move || memgardend::embed::Embedder::load(&cfg)).await??;
    *state.embedder.write().expect("embedder lock poisoned") = Some(Arc::new(embedder));
    Ok(())
}

/// CE-11. `rerank::load_at_startup` would swallow a failure into a log line
/// and leave the run silently measuring the baseline again under a "reranked"
/// label, so the load is done here where it can fail the process.
async fn load_reranker(state: &AppState) -> anyhow::Result<()> {
    let cfg = state.cfg.reranker.clone();
    let model_dir = state.cfg.embedding.model_dir.clone();
    let reranker =
        tokio::task::spawn_blocking(move || memgardend::rerank::Reranker::load(&cfg, &model_dir))
            .await??;
    *state.reranker.write().expect("reranker lock poisoned") = Some(Arc::new(reranker));
    Ok(())
}

const USAGE: &str = "\
usage:
  recall_bench import <corpus.jsonl> <db-path>
  recall_bench bench  <db-path> <gold.jsonl> <corpus.jsonl> [results.jsonl] [rerank=<top_k>]

`rerank=<top_k>` turns CE-11's cross-encoder on. Everything else about the
measurement is pinned to the AX-2 baseline's configuration and is not
adjustable, because a delta computed under a different configuration is not a
delta.";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // Positional args, no clap. Two subcommands with fixed arity do not need
    // an argument parser, and the workspace pins are frozen. The one flag
    // (CE-11's `rerank=<top_k>`) is pulled out first so the positionals keep
    // their fixed arity.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut rerank_top_k: Option<usize> = None;
    if let Some(i) = args.iter().position(|a| a.starts_with("rerank=")) {
        let raw = args.remove(i);
        let value = raw.trim_start_matches("rerank=");
        rerank_top_k = Some(
            value
                .parse()
                .map_err(|_| anyhow::anyhow!("rerank= wants a positive integer, got {value:?}"))?,
        );
        anyhow::ensure!(
            rerank_top_k != Some(0),
            "rerank=0 is not 'off'; omit the flag"
        );
    }
    let path = |i: usize| PathBuf::from(&args[i]);
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["import", _, _] => import(&path(1), &path(2)).await,
        ["bench", _, _, _] => bench(&path(1), &path(2), &path(3), None, rerank_top_k).await,
        ["bench", _, _, _, _] => {
            bench(&path(1), &path(2), &path(3), Some(&path(4)), rerank_top_k).await
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(uuid: &str, grade: u8) -> GoldLabel {
        GoldLabel {
            uuid: uuid.to_string(),
            grade,
            why: "test".to_string(),
        }
    }

    /// The worked example that pins the convention the doc comment claims:
    /// one grade-2 at rank 1 and one grade-1 at rank 3, nothing else relevant.
    #[test]
    fn ndcg_matches_the_hand_computed_burges_value() {
        let graded = [2, 0, 1];
        let dcg = 3.0 / 1.0 + 0.0 + 1.0 / 4f64.log2();
        let idcg = 3.0 / 1.0 + 1.0 / 3f64.log2();
        assert!((ndcg_at_k(&graded, &[2, 1], 10) - dcg / idcg).abs() < 1e-12);
    }

    #[test]
    fn ndcg_is_one_for_the_ideal_ordering_and_zero_with_no_labels() {
        assert!((ndcg_at_k(&[2, 1, 0], &[2, 1], 10) - 1.0).abs() < 1e-12);
        assert_eq!(ndcg_at_k(&[0, 0], &[0], 10), 0.0);
    }

    /// A query with more relevant nodes than `k` must still be able to score
    /// 1.0 — the ideal list is truncated at `k` too.
    #[test]
    fn ndcg_ideal_truncates_at_k() {
        let all = vec![2u8; 20];
        let retrieved = vec![2u8; 10];
        assert!((ndcg_at_k(&retrieved, &all, 10) - 1.0).abs() < 1e-12);
    }

    fn ledger_line(sha: &str, rerank: &str, recall10: f64) -> String {
        format!(
            r#"{{"corpus":{{"sha256":"{sha}"}},"config":{{"rerank_top_k":{rerank}}},"commit":"deadbeefcafe","overall":{{"recall@10":{recall10}}}}}"#
        )
    }

    /// The regression this guard exists for: the ledger holds an older row at
    /// 0.3881 and a newer one at 0.3792 for the same corpus and configuration,
    /// and the baseline is the newer one.
    #[test]
    fn a_later_ledger_row_supersedes_an_earlier_one() {
        let text = format!(
            "{}\n{}\n",
            ledger_line("abc", "null", 0.3881),
            ledger_line("abc", "null", 0.3792)
        );
        let (line, row) = latest_matching(&text, "abc", None).expect("a match");
        assert_eq!(line, 2);
        assert_eq!(row["overall"]["recall@10"].as_f64(), Some(0.3792));
    }

    /// A CE-11 run is not the baseline run's baseline, and vice versa.
    #[test]
    fn ledger_rows_are_matched_on_corpus_and_rerank_depth() {
        let text = format!(
            "{}\n{}\n{}\n",
            ledger_line("abc", "null", 0.3792),
            ledger_line("abc", "10", 0.3363),
            ledger_line("other", "null", 0.9)
        );
        assert_eq!(
            latest_matching(&text, "abc", None).map(|(l, _)| l),
            Some(1),
            "the rerank row must not be read as the baseline"
        );
        assert_eq!(
            latest_matching(&text, "abc", Some(10)).map(|(l, _)| l),
            Some(2)
        );
        assert!(
            latest_matching(&text, "abc", Some(20)).is_none(),
            "a depth never run has no baseline"
        );
        assert!(
            latest_matching(&text, "unknown-corpus", None).is_none(),
            "a different corpus is a different measurement"
        );
    }

    /// A ledger row carrying none of the aggregates must not be reported as
    /// reproduced — the vacuous-truth version of the mistake this guard is
    /// for. `compare_to_ledger` prints the "not a baseline" branch instead;
    /// what is asserted here is that such a row is still *selected*, so the
    /// message names a real line.
    #[test]
    fn a_row_without_aggregates_is_still_selected() {
        let text = r#"{"corpus":{"sha256":"abc"},"config":{"rerank_top_k":null},"commit":"c"}"#;
        let (line, row) = latest_matching(text, "abc", None).expect("a match");
        assert_eq!(line, 1);
        assert!(row["overall"]["mrr"].as_f64().is_none());
    }

    /// A half-written line must not shift the line numbers of the rows after
    /// it — the number is printed so a person can open the file at it.
    #[test]
    fn an_unparseable_ledger_line_is_skipped_but_still_counted() {
        let text = format!(
            "{}\nnot json\n{}\n",
            ledger_line("abc", "null", 0.1),
            ledger_line("abc", "null", 0.2)
        );
        assert_eq!(latest_matching(&text, "abc", None).map(|(l, _)| l), Some(3));
        assert_eq!(latest_matching("", "abc", None), None);
    }

    #[test]
    fn recall_denominator_is_the_labelled_relevant_set() {
        let labels = [label("a", 2), label("b", 1), label("c", 0)];
        let retrieved: Vec<String> = ["z", "a", "b"].iter().map(|s| s.to_string()).collect();
        let m = QueryMetrics::compute(&retrieved, &labels);
        assert_eq!(m.recall_1, 0.0, "rank 1 is an unlabelled node");
        assert_eq!(m.recall_5, 1.0, "both relevant nodes are inside 5");
        assert!((m.mrr - 0.5).abs() < 1e-12, "first relevant is at rank 2");
    }

    #[test]
    fn mrr_is_zero_when_nothing_relevant_is_retrieved() {
        let labels = [label("a", 2)];
        let retrieved = vec!["x".to_string()];
        let m = QueryMetrics::compute(&retrieved, &labels);
        assert_eq!(m.mrr, 0.0);
        assert_eq!(m.recall_10, 0.0);
    }

    #[test]
    fn iso_ms_parses_the_legacy_timestamp_format() {
        assert_eq!(
            iso_ms(&Some("2026-08-02T17:44:19.821668+00:00".to_string())),
            Some(1_785_692_659_821)
        );
        assert_eq!(iso_ms(&None), None);
        assert_eq!(iso_ms(&Some(String::new())), None);
    }
}
