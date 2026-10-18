//! Retrieval strategies fused by hybrid recall.
//!
//! Each strategy ranks memories for a query by its own method and owns
//! everything that method needs: its index, how many raw candidates it
//! scans, and how it handles its own failures. Hybrid search only asks each
//! strategy for `limit` memories and fuses the [`RankedList`]s it gets back.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use super::{
    fusion::{MatchSource, RankedList},
    rank_candidates, CandidateAllowList, LexicalIndex,
};
use crate::{
    embedding::EmbeddingBackend, error::MemoryError, index::VectorStore,
    repo::traced_spawn_blocking, types::ScopeFilter,
};

/// A way of ranking memories for a recall query.
///
/// A strategy returns whole-memory hits keyed by canonical index key, each
/// with the strategy's distance or score. No strategy ranks pieces of a
/// memory yet. This trait does not decide where piece-level hits collapse
/// to their parent: ADR-0042 fuses at `FactId` and collapses after fusion.
/// The PR that adds the first piece-ranking strategy settles the order and
/// amends ADR-0042 if it changes.
#[async_trait]
pub(crate) trait RetrievalStrategy: Send + Sync {
    /// Rank up to `limit` memories for `query` within `filter`.
    ///
    /// With `allowed`, only memories in the allow-list are ranked, and the
    /// strategy widens its own search window until it holds `limit` allowed
    /// memories or has covered its whole index (see
    /// [`CandidateAllowList`]). The strategy decides how many raw candidates
    /// to scan to fill `limit`; callers see only the ranked memories.
    ///
    /// An `Err` fails the whole recall. A strategy whose failures should
    /// only degrade recall handles them itself and returns an empty list.
    async fn retrieve(
        &self,
        filter: &ScopeFilter,
        query: &str,
        limit: usize,
        allowed: Option<&CandidateAllowList>,
    ) -> Result<RankedList, MemoryError>;
}

/// Embedding-similarity retrieval over the vector index.
///
/// The query is embedded, then the vector index is searched. Embedding and
/// index failures are returned as errors, so a semantic failure fails
/// recall (ADR-0038).
pub(crate) struct SemanticStrategy<'a> {
    embedding: &'a dyn EmbeddingBackend,
    index: &'a dyn VectorStore,
}

impl<'a> SemanticStrategy<'a> {
    pub(crate) fn new(embedding: &'a dyn EmbeddingBackend, index: &'a dyn VectorStore) -> Self {
        Self { embedding, index }
    }
}

#[async_trait]
impl RetrievalStrategy for SemanticStrategy<'_> {
    async fn retrieve(
        &self,
        filter: &ScopeFilter,
        query: &str,
        limit: usize,
        allowed: Option<&CandidateAllowList>,
    ) -> Result<RankedList, MemoryError> {
        let start = std::time::Instant::now();
        let query_vector = self.embedding.embed_one(query).await?;
        info!(embed_ms = start.elapsed().as_millis(), "query embedded");

        let start = std::time::Instant::now();
        let search = |fetch| {
            self.index.search(filter, &query_vector, fetch).map(|hits| {
                hits.into_iter()
                    .map(|(_key, qualified_name, distance)| (qualified_name, distance))
                    .collect::<Vec<_>>()
            })
        };
        let bound = || self.index.search_bound(filter);
        let hits = rank_candidates(allowed, limit, search, bound)?;
        info!(
            search_ms = start.elapsed().as_millis(),
            candidates = hits.len(),
            "semantic index searched"
        );
        Ok(RankedList::from_pairs(MatchSource::Semantic, hits))
    }
}

/// BM25 keyword retrieval over the in-RAM [`LexicalIndex`].
///
/// Searches run on the blocking pool. A failed search — including the
/// errors a degraded index returns until its repair converges (ADR-0039) —
/// is logged and yields an empty list, so recall serves semantic results
/// alone instead of failing (ADR-0038).
pub(crate) struct LexicalStrategy {
    index: Arc<LexicalIndex>,
}

impl LexicalStrategy {
    pub(crate) fn new(index: Arc<LexicalIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl RetrievalStrategy for LexicalStrategy {
    async fn retrieve(
        &self,
        filter: &ScopeFilter,
        query: &str,
        limit: usize,
        allowed: Option<&CandidateAllowList>,
    ) -> Result<RankedList, MemoryError> {
        let index = Arc::clone(&self.index);
        let filter = filter.clone();
        let query = query.to_string();
        let allowed = allowed.cloned();
        let result = traced_spawn_blocking(move || {
            let search = |fetch| index.search(&filter, &query, fetch);
            let bound = || index.search_bound();
            rank_candidates(allowed.as_ref(), limit, search, bound)
        })
        .await;
        let hits = match result {
            Ok(Ok(hits)) => hits,
            Ok(Err(e)) => {
                warn!(error = %e, "lexical search failed — returning semantic results only");
                Vec::new()
            }
            Err(e) => {
                warn!(error = %e, "lexical search task failed — returning semantic results only");
                Vec::new()
            }
        };
        Ok(RankedList::from_pairs(MatchSource::Lexical, hits))
    }
}
