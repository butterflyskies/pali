//! Integration tests for hybrid (semantic + lexical) retrieval.
//!
//! The golden test recreates a real recall miss (issue #55): a memory whose
//! ~3 KB of multi-topic content buries the literal phrase "happy birthday"
//! mid-document. Whole-memory embeddings dilute the phrase, so pure semantic
//! recall ranks topically-adjacent decoys above it and the memory falls out
//! of the top-k entirely. Hybrid retrieval must surface it via BM25.

use std::sync::Arc;

use async_trait::async_trait;
use pali::{
    embedding::EmbeddingBackend,
    error::MemoryError,
    index::{InMemoryStore, VectorStore},
    search::{hybrid_search, LexicalIndex},
    types::{MemoryName, MemoryRef, Scope, ScopeFilter, ScopePath},
};

const DIMS: usize = 4;

/// Embedding backend that always returns `vector` — used to position the
/// *query* in vector space. Memory vectors are inserted into the store
/// directly, so this is only consulted by `hybrid_search` for the query.
struct FixedQueryEmbedding {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbeddingBackend for FixedQueryEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        DIMS
    }
}

/// Embedding backend that always fails — simulates an embedding outage.
struct FailingEmbedding;

#[async_trait]
impl EmbeddingBackend for FailingEmbedding {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Err(MemoryError::Embedding("worker unavailable".to_string()))
    }

    fn dimensions(&self) -> usize {
        DIMS
    }
}

fn key(scope: &Scope, name: &str) -> String {
    MemoryRef::new(scope.clone(), MemoryName::new(name.to_string()).unwrap()).qualified_path()
}

/// A ~3 KB multi-topic "communication style" memory with the literal phrase
/// "happy birthday" buried mid-document. Mirrors the shape of the real
/// memory that pure semantic recall missed.
fn golden_content() -> String {
    let mut content = String::new();
    content.push_str("# Communication style notes\n\n## Cadence\n");
    for i in 0..12 {
        content.push_str(&format!(
            "Prefers short direct messages over long explanations; thread {i} showed that \
             replies land better when they lead with the outcome and skip the preamble. \
             Match her cadence, do not restate shared context, and avoid filler words.\n"
        ));
    }
    content.push_str("\n## Greetings\n");
    content.push_str(
        "\"happy birthday\" = her pre-coffee good morning. more polite than saying grrr.\n",
    );
    content.push_str("\n## Timing\n");
    for i in 0..12 {
        content.push_str(&format!(
            "Mornings are slow-start; substantive questions get better answers after coffee. \
             Observation {i}: pings before 9am get one-word replies, so batch non-urgent \
             items into a single message and send it mid-morning instead.\n"
        ));
    }
    assert!(
        content.len() > 3000,
        "golden memory should be ~3KB+, got {}",
        content.len()
    );
    content
}

/// Topically-adjacent decoy contents: greetings, birthdays, celebrations,
/// mornings — but never the literal phrase "happy birthday".
fn decoy_contents() -> Vec<String> {
    vec![
        "Birthday party planning for the group: cake, venue, and a shared gift.".to_string(),
        "Calendar preferences: birthdays and anniversaries synced from Fastmail.".to_string(),
        "Morning greeting rituals differ per person; some want silence until coffee.".to_string(),
        "Celebration etiquette: congratulate promptly, keep it short and warm.".to_string(),
        "Happy hour scheduling notes: Thursdays work best for most of the group.".to_string(),
        "Gift ideas list, updated quarterly before each birthday season.".to_string(),
        "Greeting card templates for holidays and special occasions.".to_string(),
        "Anniversary reminders should fire a week early to allow gift shipping.".to_string(),
        "Cron definition: morning thought delivered to Lina at 8am local.".to_string(),
        "Cron definition: haiku posted daily; skip when the channel is busy.".to_string(),
        "Party playlist collaboration doc lives in the shared drive.".to_string(),
        "Cake preferences: chocolate wins, no fondant, candles optional.".to_string(),
    ]
}

/// Build the miss scenario: 12 decoys sit close to the query vector, the
/// golden memory sits far away, and only the golden memory contains the
/// literal phrase. With `limit = 10`, semantic-only recall cannot return
/// the golden memory at all.
fn build_miss_scenario(
    scope: &Scope,
) -> (
    FixedQueryEmbedding,
    InMemoryStore,
    Arc<LexicalIndex>,
    String,
) {
    let store = InMemoryStore::new(DIMS);
    let lexical = Arc::new(LexicalIndex::new());

    let golden_key = key(scope, "person-lina-communication");
    // Golden memory: far from the query vector (its embedding is dominated
    // by the surrounding multi-topic prose, not the buried phrase).
    store
        .add(scope, &[0.0, 0.0, 0.0, 1.0], golden_key.clone())
        .expect("add golden");
    lexical
        .upsert(&golden_key, "person-lina-communication", &golden_content())
        .expect("upsert golden");

    // Decoys: clustered around the query vector, each slightly different.
    for (i, content) in decoy_contents().iter().enumerate() {
        let name = format!("decoy-{i}");
        let decoy_key = key(scope, &name);
        #[allow(clippy::cast_precision_loss)]
        let jitter = 0.01 * (i as f32 + 1.0);
        store
            .add(scope, &[1.0, jitter, 0.0, 0.0], decoy_key.clone())
            .expect("add decoy");
        lexical
            .upsert(&decoy_key, &name, content)
            .expect("upsert decoy");
    }

    let embedding = FixedQueryEmbedding {
        vector: vec![1.0, 0.0, 0.0, 0.0],
    };
    (embedding, store, lexical, golden_key)
}

// ---------------------------------------------------------------------------
// Golden test — the "happy birthday" miss
// ---------------------------------------------------------------------------

#[tokio::test]
async fn golden_buried_phrase_surfaces_in_top_results() {
    let scope = Scope::Root;
    let (embedding, store, lexical, golden_key) = build_miss_scenario(&scope);

    // Precondition: semantic-only search misses the golden memory entirely.
    let query_vector = embedding.embed_one("happy birthday").await.unwrap();
    let semantic_only = store
        .search(&ScopeFilter::RootOnly, &query_vector, 10)
        .expect("semantic search");
    assert_eq!(semantic_only.len(), 10, "10 decoys fill the semantic top-k");
    assert!(
        !semantic_only.iter().any(|(_, name, _)| name == &golden_key),
        "precondition: pure semantic recall must miss the golden memory"
    );

    // Hybrid search must surface it.
    let hits = hybrid_search(
        &embedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "happy birthday",
        10,
    )
    .await
    .expect("hybrid search");

    let position = hits
        .iter()
        .position(|h| h.qualified_name == golden_key)
        .expect("golden memory must appear in hybrid results");
    // RRF bound: as the top lexical hit (exact phrase match), the golden
    // memory beats every semantic-only candidate; only candidates returned
    // by BOTH strategies can outrank it. Exactly three decoys contain a
    // bare "birthday"/"happy" term, so the worst possible rank is 3.
    assert!(
        position <= 3,
        "golden memory must outrank all semantic-only decoys, got rank {position}"
    );

    let golden_hit = &hits[position];
    assert_eq!(
        golden_hit.match_type(),
        "lexical",
        "the golden memory is only reachable via the lexical strategy"
    );
    assert_eq!(golden_hit.semantic_distance, None);
    assert!(golden_hit.lexical_score.is_some());
}

// ---------------------------------------------------------------------------
// Scope filtering parity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hybrid_respects_scope_filter() {
    let root = Scope::Root;
    let project = Scope::Path(ScopePath::new("other-project").unwrap());

    let store = InMemoryStore::new(DIMS);
    let lexical = Arc::new(LexicalIndex::new());

    // Same phrase in both scopes; only the root memory may be returned
    // for a RootOnly filter — via either strategy.
    for (scope, name) in [(&root, "root-note"), (&project, "project-note")] {
        let k = key(scope, name);
        store
            .add(scope, &[0.0, 0.0, 0.0, 1.0], k.clone())
            .expect("add");
        lexical
            .upsert(&k, name, "the launch codeword is xyzzy, do not lose it")
            .expect("upsert");
    }

    let embedding = FixedQueryEmbedding {
        vector: vec![1.0, 0.0, 0.0, 0.0],
    };

    let hits = hybrid_search(
        &embedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "xyzzy",
        10,
    )
    .await
    .expect("hybrid search");

    assert_eq!(hits.len(), 1, "only the root-scoped memory may be returned");
    assert_eq!(hits[0].qualified_name, key(&root, "root-note"));
}

// ---------------------------------------------------------------------------
// Agreement between strategies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hit_found_by_both_strategies_is_marked_both_and_ranks_first() {
    let scope = Scope::Root;
    let store = InMemoryStore::new(DIMS);
    let lexical = Arc::new(LexicalIndex::new());

    // "agreed" is both semantically nearest AND the only keyword match.
    let agreed_key = key(&scope, "agreed");
    store
        .add(&scope, &[1.0, 0.0, 0.0, 0.0], agreed_key.clone())
        .expect("add");
    lexical
        .upsert(&agreed_key, "agreed", "the xyzzy incident report")
        .expect("upsert");

    let other_key = key(&scope, "other");
    store
        .add(&scope, &[0.9, 0.1, 0.0, 0.0], other_key.clone())
        .expect("add");
    lexical
        .upsert(&other_key, "other", "unrelated grocery list")
        .expect("upsert");

    let embedding = FixedQueryEmbedding {
        vector: vec![1.0, 0.0, 0.0, 0.0],
    };

    let hits = hybrid_search(
        &embedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "xyzzy",
        10,
    )
    .await
    .expect("hybrid search");

    assert_eq!(hits[0].qualified_name, agreed_key);
    assert_eq!(hits[0].match_type(), "both");
    assert!(hits[0].semantic_distance.is_some());
    assert!(hits[0].lexical_score.is_some());
}

// ---------------------------------------------------------------------------
// Error surface
// ---------------------------------------------------------------------------

/// Degraded lexical index (ADR-0039): hybrid recall must succeed with
/// semantic-only results — the lexical strategy errors instead of serving
/// potentially stale hits, and fusion degrades gracefully.
#[tokio::test]
async fn degraded_lexical_index_serves_semantic_only() {
    let scope = Scope::Root;
    let (embedding, store, lexical, golden_key) = build_miss_scenario(&scope);

    // Sanity: while healthy, the lexical strategy surfaces the golden hit.
    let healthy = hybrid_search(
        &embedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "happy birthday",
        10,
    )
    .await
    .expect("hybrid search");
    assert!(healthy.iter().any(|h| h.qualified_name == golden_key));

    // A divergence event flags the index; recall serves semantic-only.
    lexical.mark_rebuild_required("test: forced divergence");

    let degraded = hybrid_search(
        &embedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "happy birthday",
        10,
    )
    .await
    .expect("hybrid search must survive a degraded lexical index");

    assert!(
        !degraded.is_empty(),
        "semantic results must still be served"
    );
    assert!(
        degraded.iter().all(|h| h.match_type() == "semantic"),
        "no lexical hits may be served while degraded"
    );
    assert!(
        !degraded.iter().any(|h| h.qualified_name == golden_key),
        "the lexical-only golden hit must disappear while degraded"
    );
}

#[tokio::test]
async fn semantic_failure_is_fatal_preserving_recall_error_surface() {
    let store = InMemoryStore::new(DIMS);
    let lexical = Arc::new(LexicalIndex::new());

    let result = hybrid_search(
        &FailingEmbedding,
        &store,
        &lexical,
        &ScopeFilter::RootOnly,
        "anything",
        10,
    )
    .await;

    assert!(
        matches!(result, Err(MemoryError::Embedding(_))),
        "an embedding failure must propagate, not silently degrade"
    );
}
