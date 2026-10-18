//! Test-only oracle for hybrid retrieval: a frozen copy of the semantic +
//! lexical wiring and the two-list reciprocal rank fusion, kept verbatim so
//! the production path can be checked against it.
//!
//! The property test below generates small stores and queries — with
//! deliberately colliding vectors, documents, and ranks so every tie-break
//! is exercised — and requires [`hybrid_search_within`] to return exactly
//! the same hits, in the same order, with the same distances, BM25 scores,
//! and fused scores as this copy. It stays as a regression oracle: any later
//! change to the production wiring or fusion that alters results shows up as
//! a difference from this copy.
//!
//! The copy shares [`rank_candidates`] (the allow-list window search) with
//! production; only the wiring and fusion are frozen here.

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use super::{hybrid_search_within, rank_candidates, CandidateAllowList, FusedHit, LexicalIndex};
use crate::{
    embedding::EmbeddingBackend,
    error::MemoryError,
    index::{InMemoryStore, VectorStore},
    types::{MemoryName, MemoryRef, Scope, ScopeFilter, ScopePath},
};

const RRF_K: f64 = 60.0;

fn rrf_contribution(rank: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let rank = rank as f64;
    1.0 / (RRF_K + rank + 1.0)
}

/// Frozen two-list fusion: semantic list first, then lexical.
fn reference_fusion(
    semantic: &[(String, f32)],
    lexical: &[(String, f32)],
    limit: usize,
) -> Vec<FusedHit> {
    let mut merged: HashMap<&str, FusedHit> = HashMap::new();

    for (rank, (qualified_name, distance)) in semantic.iter().enumerate() {
        merged
            .entry(qualified_name.as_str())
            .or_insert_with(|| FusedHit {
                qualified_name: qualified_name.clone(),
                semantic_distance: Some(*distance),
                lexical_score: None,
                score: rrf_contribution(rank),
            });
    }

    for (rank, (qualified_name, bm25)) in lexical.iter().enumerate() {
        merged
            .entry(qualified_name.as_str())
            .and_modify(|hit| {
                if hit.lexical_score.is_none() {
                    hit.lexical_score = Some(*bm25);
                    hit.score += rrf_contribution(rank);
                }
            })
            .or_insert_with(|| FusedHit {
                qualified_name: qualified_name.clone(),
                semantic_distance: None,
                lexical_score: Some(*bm25),
                score: rrf_contribution(rank),
            });
    }

    let mut hits: Vec<FusedHit> = merged.into_values().collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.lexical_score.is_some().cmp(&a.lexical_score.is_some()))
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
    });
    hits.truncate(limit);
    hits
}

/// Frozen hybrid wiring: semantic errors are fatal, lexical errors degrade
/// to an empty lexical list.
async fn reference_hybrid_search(
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
    lexical: &LexicalIndex,
    filter: &ScopeFilter,
    query: &str,
    limit: usize,
    allowed: Option<&CandidateAllowList>,
) -> Result<Vec<FusedHit>, MemoryError> {
    if allowed.is_some_and(|allowed| allowed.names.is_empty()) {
        return Ok(Vec::new());
    }
    let query_vector = embedding.embed_one(query).await?;
    let semantic = rank_candidates(
        allowed,
        limit,
        |fetch| {
            index.search(filter, &query_vector, fetch).map(|hits| {
                hits.into_iter()
                    .map(|(_key, qualified_name, distance)| (qualified_name, distance))
                    .collect::<Vec<_>>()
            })
        },
        || index.search_bound(filter),
    )?;
    let lexical = rank_candidates(
        allowed,
        limit,
        |fetch| lexical.search(filter, query, fetch),
        || lexical.search_bound(),
    )
    .unwrap_or_default();
    Ok(reference_fusion(&semantic, &lexical, limit))
}

const DIMS: usize = 4;
const QUERY_VECTOR: [f32; DIMS] = [1.0, 0.0, 0.0, 0.0];

/// A few fixed directions, so generated memories collide on distance.
const VECTORS: [[f32; DIMS]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [1.0, 1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [1.0, 0.0, 1.0, 0.0],
];

/// A tiny vocabulary, so generated memories collide on BM25 score.
const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

struct FixedQueryEmbedding;

#[async_trait::async_trait]
impl EmbeddingBackend for FixedQueryEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(texts.iter().map(|_| QUERY_VECTOR.to_vec()).collect())
    }

    fn dimensions(&self) -> usize {
        DIMS
    }
}

#[derive(Debug, Clone)]
struct GenMemory {
    in_team_scope: bool,
    vector: Option<usize>,
    words: Option<Vec<usize>>,
    allowed: bool,
}

fn memory_strategy() -> impl Strategy<Value = GenMemory> {
    (
        any::<bool>(),
        proptest::option::weighted(0.8, 0..VECTORS.len()),
        proptest::option::weighted(0.8, proptest::collection::vec(0..WORDS.len(), 1..4)),
        any::<bool>(),
    )
        .prop_map(|(in_team_scope, vector, words, allowed)| GenMemory {
            in_team_scope,
            vector,
            words,
            allowed,
        })
}

fn filter_strategy() -> impl Strategy<Value = ScopeFilter> {
    prop_oneof![
        Just(ScopeFilter::RootOnly),
        Just(ScopeFilter::All),
        Just(ScopeFilter::Subtree(
            ScopePath::new("team").expect("valid scope path")
        )),
    ]
}

fn words(indices: &[usize]) -> String {
    indices
        .iter()
        .map(|&index| WORDS[index])
        .collect::<Vec<_>>()
        .join(" ")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Recall's hybrid search returns exactly the oracle's fused hits — same
    /// order, ties included — for generated stores, queries, scope filters,
    /// limits, and allow-lists.
    #[test]
    fn hybrid_search_matches_the_reference_wiring(
        memories in proptest::collection::vec(memory_strategy(), 0..10),
        query in proptest::collection::vec(0..WORDS.len(), 1..3),
        filter in filter_strategy(),
        limit in 1usize..8,
        prefilter in proptest::option::of(0usize..12),
    ) {
        let store = InMemoryStore::new(DIMS);
        let lexical = LexicalIndex::new();
        let mut allow_names = HashSet::new();
        for (index, memory) in memories.iter().enumerate() {
            let scope = if memory.in_team_scope {
                Scope::Path(ScopePath::new("team").expect("valid scope path"))
            } else {
                Scope::Root
            };
            let name = format!("m{index}");
            let key = MemoryRef::new(
                scope.clone(),
                MemoryName::new(name.clone()).expect("valid name"),
            )
            .qualified_path();
            if let Some(vector) = memory.vector {
                store.add(&scope, &VECTORS[vector], key.clone()).expect("add vector");
            }
            if let Some(content) = &memory.words {
                lexical.upsert(&key, &name, &words(content)).expect("upsert");
            }
            if memory.allowed {
                allow_names.insert(key);
            }
        }
        let allowed = prefilter.map(|fetch| CandidateAllowList::new(allow_names, fetch));
        let query = words(&query);
        let lexical = std::sync::Arc::new(lexical);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let (actual, expected) = runtime.block_on(async {
            let actual = hybrid_search_within(
                &FixedQueryEmbedding,
                &store,
                &lexical,
                &filter,
                &query,
                limit,
                allowed.as_ref(),
            )
            .await
            .expect("hybrid search");
            let expected = reference_hybrid_search(
                &FixedQueryEmbedding,
                &store,
                &lexical,
                &filter,
                &query,
                limit,
                allowed.as_ref(),
            )
            .await
            .expect("reference search");
            (actual, expected)
        });
        prop_assert_eq!(actual, expected);
    }
}
