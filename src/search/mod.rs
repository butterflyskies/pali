//! Hybrid retrieval — semantic (embedding) and lexical (BM25) strategies
//! run in parallel and merged via reciprocal rank fusion.
//!
//! Whole-memory embeddings dilute short exact phrases inside long
//! multi-topic memories: a one-line fact in a 3 KB memory contributes almost
//! nothing to the document vector, so literal-phrase queries rank it below
//! short memories that are topically adjacent but wrong. The lexical
//! strategy catches exactly those queries; fusion lets either strategy
//! surface a hit the other missed.

use std::{collections::HashSet, sync::Arc};

use tracing::{info, warn, Instrument};

use crate::{
    embedding::EmbeddingBackend,
    error::MemoryError,
    index::VectorStore,
    repo::{traced_spawn_blocking, MemoryRepo},
    repo_router::RepoRouter,
    types::ScopeFilter,
};

/// BM25 keyword index (Tantivy, in-RAM).
pub mod bm25;
/// Reciprocal rank fusion for merging result lists.
pub mod fusion;

pub use bm25::{LexicalDoc, LexicalIndex, LexicalOp, LexicalStatus};
pub use fusion::{reciprocal_rank_fusion, FusedHit};

/// Run semantic and lexical retrieval in parallel and merge the ranked
/// lists with reciprocal rank fusion.
///
/// Each strategy contributes up to `limit` candidates; the fused list is
/// truncated back to `limit`. A semantic failure (embedding or vector index)
/// is fatal — it preserves recall's pre-hybrid error surface. A lexical
/// failure only degrades: the error is logged and semantic results are
/// returned alone, so the new subsystem can never break recall.
pub async fn hybrid_search(
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
    lexical: &Arc<LexicalIndex>,
    filter: &ScopeFilter,
    query: &str,
    limit: usize,
) -> Result<Vec<FusedHit>, MemoryError> {
    hybrid_search_within(embedding, index, lexical, filter, query, limit, None).await
}

/// Candidate pre-filter for hybrid retrieval.
///
/// Neither index stores tags, so callers resolve a tag predicate to the
/// exact set of qualified names that may be ranked. `fetch` is the initial
/// number of in-scope candidates each strategy ranks before the allow-list
/// is applied; callers size it to the in-scope memory count so one pass
/// normally suffices. When a pass yields fewer than `limit` allowed hits
/// (index entries that outlived their memory, or other drift between the
/// index and git), the strategy re-ranks with a doubled window until it
/// finds `limit` allowed hits or the window covers the whole index, so an
/// allowed memory is never starved by higher-ranked disallowed entries.
#[derive(Debug, Clone)]
pub(crate) struct CandidateAllowList {
    names: Arc<HashSet<String>>,
    fetch: usize,
}

impl CandidateAllowList {
    pub(crate) fn new(names: HashSet<String>, fetch: usize) -> Self {
        Self {
            names: Arc::new(names),
            fetch,
        }
    }

    /// Rank through `search` until `limit` allowed hits are found or the
    /// window covers every candidate, then keep the best `limit`.
    ///
    /// Hits are deduplicated by name, keeping each name's best-ranked row.
    /// The search stops once it holds `limit` distinct allowed names or
    /// every allowed name. Otherwise the window doubles until it reaches
    /// `bound`, the
    /// strategy's raw entry count (re-read after each pass). Only that raw
    /// bound proves exhaustion: the hits a store returns are not a signal,
    /// since a store may drop raw entries before returning (usearch drops
    /// keys that no longer map to a memory), so passes can come back short,
    /// or the same length as a narrower pass, while candidates lie behind
    /// them.
    fn rank(
        &self,
        limit: usize,
        mut search: impl FnMut(usize) -> Result<Vec<(String, f32)>, MemoryError>,
        bound: impl Fn() -> usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        let mut fetch = self.fetch.max(limit).max(1);
        loop {
            let mut hits = search(fetch)?;
            let exhausted = fetch >= bound();
            // Hits are rank-ordered, so dropping disallowed entries and later
            // duplicates of a name, then cutting to `limit`, keeps this
            // strategy's best allowed hits. A store may return several raw
            // keys for one name (usearch keeps an old key mapped when an
            // upsert fails to remove it), so rows are not names: the stops
            // below must count distinct names, or duplicates fill `limit`
            // ahead of a later allowed name that fusion then never sees.
            let mut seen = HashSet::new();
            hits.retain(|(qualified_name, _)| {
                self.names.contains(qualified_name) && seen.insert(qualified_name.clone())
            });
            // Hits are now distinct names, so holding every allowed name
            // means no wider pass can add one.
            if exhausted || hits.len() >= limit.min(self.names.len()) {
                hits.truncate(limit);
                return Ok(hits);
            }
            fetch = fetch.saturating_mul(2);
        }
    }
}

/// Rank one strategy's candidates: through the allow-list when there is
/// one, otherwise a single `limit`-sized pass. `bound` is the strategy's
/// raw entry count (see [`CandidateAllowList::rank`]).
fn rank_candidates(
    allowed: Option<&CandidateAllowList>,
    limit: usize,
    mut search: impl FnMut(usize) -> Result<Vec<(String, f32)>, MemoryError>,
    bound: impl Fn() -> usize,
) -> Result<Vec<(String, f32)>, MemoryError> {
    match allowed {
        Some(allowed) => allowed.rank(limit, search, bound),
        None => search(limit),
    }
}

/// [`hybrid_search`] restricted to an optional candidate allow-list.
///
/// With an allow-list, each strategy ranks in-scope candidates in growing
/// windows (see [`CandidateAllowList`]), drops every candidate outside the
/// list, and only then cuts to `limit` and fuses — a pre-filter, so `limit`
/// allowed memories are returned whenever that many match, however highly
/// disallowed memories rank.
pub(crate) async fn hybrid_search_within(
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
    lexical: &Arc<LexicalIndex>,
    filter: &ScopeFilter,
    query: &str,
    limit: usize,
    allowed: Option<&CandidateAllowList>,
) -> Result<Vec<FusedHit>, MemoryError> {
    let span = tracing::info_span!(
        "search.hybrid",
        ?filter,
        limit,
        prefiltered = allowed.is_some(),
        semantic_candidates = tracing::field::Empty,
        lexical_candidates = tracing::field::Empty,
    );
    async move {
        if allowed.is_some_and(|allowed| allowed.names.is_empty()) {
            return Ok(Vec::new());
        }
        let semantic_fut = async {
            let start = std::time::Instant::now();
            let query_vector = embedding.embed_one(query).await?;
            info!(embed_ms = start.elapsed().as_millis(), "query embedded");

            let start = std::time::Instant::now();
            let search = |fetch| {
                index.search(filter, &query_vector, fetch).map(|hits| {
                    hits.into_iter()
                        .map(|(_key, qualified_name, distance)| (qualified_name, distance))
                        .collect::<Vec<_>>()
                })
            };
            let bound = || index.search_bound(filter);
            let results = rank_candidates(allowed, limit, search, bound)?;
            info!(
                search_ms = start.elapsed().as_millis(),
                candidates = results.len(),
                "semantic index searched"
            );
            Ok::<_, MemoryError>(results)
        };

        let lexical_index = Arc::clone(lexical);
        let lexical_filter = filter.clone();
        let lexical_query = query.to_string();
        let lexical_allowed = allowed.cloned();
        let lexical_fut = traced_spawn_blocking(move || {
            let search = |fetch| lexical_index.search(&lexical_filter, &lexical_query, fetch);
            let bound = || lexical_index.search_bound();
            rank_candidates(lexical_allowed.as_ref(), limit, search, bound)
        });

        let (semantic_result, lexical_result) = tokio::join!(semantic_fut, lexical_fut);

        let semantic = semantic_result?;
        let lexical: Vec<(String, f32)> = match lexical_result {
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

        tracing::Span::current().record("semantic_candidates", semantic.len());
        tracing::Span::current().record("lexical_candidates", lexical.len());

        Ok(reciprocal_rank_fusion(&semantic, &lexical, limit))
    }
    .instrument(span)
    .await
}

/// Rebuild the lexical index from every memory in the repository.
///
/// The lexical index lives in RAM only, so this runs on every startup —
/// unlike the vector index, which is persisted because embedding is
/// expensive. The rebuild is a single Tantivy commit and runs on the
/// blocking pool. Returns the number of indexed memories.
///
/// This is also the repair path for a degraded index (ADR-0039): the
/// rebuild token is captured *before* the repo listing, so divergence
/// events or mirrors racing the listing keep the index flagged and a
/// follow-up rebuild converges instead of silently losing them.
///
/// Every failure while obtaining or applying repository truth — including
/// the repo listing itself, *before* the `rebuild_from` seam — marks the
/// index rebuild-required. Without that, a listing failure on a fresh
/// index would leave the epochs at 0/0: healthy-but-empty, with recall
/// never scheduling repair (the #314 pre-list gap).
pub async fn rebuild_lexical_from_repo(
    repo: &Arc<MemoryRepo>,
    lexical: &Arc<LexicalIndex>,
) -> Result<usize, MemoryError> {
    let token = lexical.begin_rebuild();
    let memories = match repo.list_memories(None).await {
        Ok(memories) => memories,
        Err(e) => {
            lexical.mark_rebuild_required("repository listing for lexical rebuild failed");
            return Err(e);
        }
    };
    let docs: Vec<LexicalDoc> = memories
        .into_iter()
        .map(|m| LexicalDoc {
            qualified_name: m.mem_ref().qualified_path(),
            name: m.name.to_string(),
            content: m.content,
        })
        .collect();
    let index = Arc::clone(lexical);
    match traced_spawn_blocking(move || index.rebuild_from(token, docs)).await {
        Ok(result) => result,
        Err(e) => {
            // The blocking rebuild task died (panic/shutdown); whether the
            // Tantivy commit landed is unknowable, so stay rebuild-required.
            lexical.mark_rebuild_required("lexical rebuild task did not run to completion");
            Err(MemoryError::Join(e.to_string()))
        }
    }
}

/// Rebuild the lexical index from every repository owned by a router.
///
/// Unlike the user-facing aggregate list operation, this is strict: failure
/// to read any mapped repository leaves the index degraded instead of
/// treating a partial aggregate as authoritative git truth.
pub async fn rebuild_lexical_from_router(
    router: &RepoRouter,
    lexical: &Arc<LexicalIndex>,
) -> Result<usize, MemoryError> {
    let token = lexical.begin_rebuild();
    let memories = match router.list_memories_strict().await {
        Ok(memories) => memories,
        Err(e) => {
            lexical.mark_rebuild_required("repository listing for lexical rebuild failed");
            return Err(e);
        }
    };
    let docs: Vec<LexicalDoc> = memories
        .into_iter()
        .map(|m| LexicalDoc {
            qualified_name: m.mem_ref().qualified_path(),
            name: m.name.to_string(),
            content: m.content,
        })
        .collect();
    let index = Arc::clone(lexical);
    match traced_spawn_blocking(move || index.rebuild_from(token, docs)).await {
        Ok(result) => result,
        Err(e) => {
            lexical.mark_rebuild_required("lexical rebuild task did not run to completion");
            Err(MemoryError::Join(e.to_string()))
        }
    }
}

/// Spawn a background repair rebuild if the lexical index is degraded and
/// no repair is already running.
///
/// Deterministic repair per ADR-0039: rebuild from git truth on the
/// blocking pool, single-flight. Recall serves semantic-only for the whole
/// degraded window (search errors until the rebuild converges). Failures
/// leave the index degraded; the next trigger retries.
pub fn spawn_lexical_repair(repo: &Arc<MemoryRepo>, lexical: &Arc<LexicalIndex>) {
    if !lexical.is_degraded() || !lexical.try_claim_repair() {
        return;
    }
    let repo = Arc::clone(repo);
    let lexical = Arc::clone(lexical);
    tokio::spawn(async move {
        let result = rebuild_lexical_from_repo(&repo, &lexical).await;
        lexical.finish_repair();
        match result {
            // `rebuild_from` deliberately returns Ok while re-flagging the
            // index when a mirror raced its repo listing (or a divergence
            // event landed mid-rebuild) — a raced outcome is not a repair
            // receipt, so the log must not claim convergence.
            Ok(count) if lexical.is_degraded() => info!(
                count,
                "lexical rebuild completed but the index was re-flagged during \
                 the rebuild (raced mirror or new divergence) — still degraded, \
                 the next trigger repairs again"
            ),
            Ok(count) => info!(count, "lexical index repaired from git truth"),
            Err(e) => warn!(error = %e, "lexical repair failed — index stays degraded"),
        }
    });
}

/// Spawn a single-flight repair from all repositories owned by a router.
pub fn spawn_lexical_repair_for_router(router: &RepoRouter, lexical: &Arc<LexicalIndex>) {
    if !lexical.is_degraded() || !lexical.try_claim_repair() {
        return;
    }
    let router = router.clone();
    let lexical = Arc::clone(lexical);
    tokio::spawn(async move {
        let result = rebuild_lexical_from_router(&router, &lexical).await;
        lexical.finish_repair();
        match result {
            Ok(count) if lexical.is_degraded() => info!(
                count,
                "lexical rebuild completed but the index was re-flagged during \
                 the rebuild (raced mirror or new divergence) — still degraded, \
                 the next trigger repairs again"
            ),
            Ok(count) => info!(count, "lexical index repaired from all git repositories"),
            Err(e) => warn!(error = %e, "lexical repair failed — index stays degraded"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        index::InMemoryStore,
        types::{MemoryName, MemoryRef, Scope},
    };
    use async_trait::async_trait;

    const DIMS: usize = 4;

    struct FixedQueryEmbedding;

    #[async_trait]
    impl EmbeddingBackend for FixedQueryEmbedding {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }

        fn dimensions(&self) -> usize {
            DIMS
        }
    }

    struct FailingEmbedding;

    #[async_trait]
    impl EmbeddingBackend for FailingEmbedding {
        async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
            Err(MemoryError::Embedding("must not be called".to_string()))
        }

        fn dimensions(&self) -> usize {
            DIMS
        }
    }

    fn key(name: &str) -> String {
        MemoryRef::new(Scope::Root, MemoryName::new(name.to_string()).unwrap()).qualified_path()
    }

    fn allow(names: &[&str], fetch: usize) -> CandidateAllowList {
        CandidateAllowList::new(names.iter().map(|name| key(name)).collect(), fetch)
    }

    /// Initial windows exercised by the pre-filter tests: one wide enough
    /// for a single pass, and one the strategy must widen.
    const WINDOWS: [(usize, &str); 2] = [(7, "single pass"), (1, "widened window")];

    // Regression: a post-filter (cut to `limit`, then drop disallowed) would
    // return nothing here because five disallowed neighbours outrank
    // `keeper`; an unwidened undersized window starved it the same way.
    #[tokio::test]
    async fn semantic_prefilter_ranks_past_disallowed_neighbours() {
        let store = InMemoryStore::new(DIMS);
        add_decoys(&store, 5);
        store
            .add(&Scope::Root, &[0.0, 0.0, 0.0, 1.0], key("keeper"))
            .expect("add keeper");
        let lexical = Arc::new(LexicalIndex::new());

        for (fetch, label) in WINDOWS {
            let hits = hybrid_search_within(
                &FixedQueryEmbedding,
                &store,
                &lexical,
                &ScopeFilter::RootOnly,
                "anything",
                1,
                Some(&allow(&["keeper"], fetch)),
            )
            .await
            .expect("search");

            let names: Vec<_> = hits.iter().map(|hit| hit.qualified_name.clone()).collect();
            assert_eq!(names, [key("keeper")], "{label}");
            assert!(hits[0].semantic_distance.is_some(), "{label}");
        }
    }

    // Regression: the BM25 path must honour the allow-list too, not only the
    // semantic one, and widen its window the same way; the decoys are the
    // stronger keyword matches.
    #[tokio::test]
    async fn lexical_prefilter_ranks_past_disallowed_hits() {
        let store = InMemoryStore::new(DIMS);
        let lexical = Arc::new(LexicalIndex::new());
        for index in 1..=5 {
            lexical
                .upsert(
                    &key(&format!("decoy-{index}")),
                    "decoy",
                    "zebra zebra zebra zebra",
                )
                .expect("upsert decoy");
        }
        lexical
            .upsert(
                &key("keeper"),
                "keeper",
                "one zebra among many other words here",
            )
            .expect("upsert keeper");

        let unfiltered = hybrid_search(
            &FixedQueryEmbedding,
            &store,
            &lexical,
            &ScopeFilter::RootOnly,
            "zebra",
            1,
        )
        .await
        .expect("search");
        assert_ne!(unfiltered[0].qualified_name, key("keeper"), "precondition");

        for (fetch, label) in WINDOWS {
            let hits = hybrid_search_within(
                &FixedQueryEmbedding,
                &store,
                &lexical,
                &ScopeFilter::RootOnly,
                "zebra",
                1,
                Some(&allow(&["keeper"], fetch)),
            )
            .await
            .expect("search");
            let names: Vec<_> = hits.iter().map(|hit| hit.qualified_name.clone()).collect();
            assert_eq!(names, [key("keeper")], "{label}");
            assert!(hits[0].lexical_score.is_some(), "{label}");
            assert!(hits[0].semantic_distance.is_none(), "{label}");
        }
    }

    #[tokio::test]
    async fn empty_allow_list_returns_nothing_without_embedding() {
        let store = InMemoryStore::new(DIMS);
        store
            .add(&Scope::Root, &[1.0, 0.0, 0.0, 0.0], key("decoy"))
            .expect("add decoy");
        let lexical = Arc::new(LexicalIndex::new());

        let hits = hybrid_search_within(
            &FailingEmbedding,
            &store,
            &lexical,
            &ScopeFilter::RootOnly,
            "anything",
            5,
            Some(&allow(&[], 1)),
        )
        .await
        .expect("an empty allow-list short-circuits before embedding");
        assert!(hits.is_empty());
    }

    fn add_decoys(store: &InMemoryStore, count: u8) {
        for index in 1..=count {
            let jitter = 0.01 * f32::from(index);
            store
                .add(
                    &Scope::Root,
                    &[1.0, jitter, 0.0, 0.0],
                    key(&format!("decoy-{index}")),
                )
                .expect("add decoy");
        }
    }

    // Regression: filtering must keep rank order and cut to `limit` only
    // after dropping disallowed entries, with several allowed candidates.
    #[tokio::test]
    async fn prefilter_keeps_the_best_allowed_hits_in_rank_order() {
        let store = InMemoryStore::new(DIMS);
        add_decoys(&store, 2);
        for (name, distance_axis) in [("near", 0.2), ("mid", 0.5), ("far", 0.9)] {
            store
                .add(&Scope::Root, &[1.0, 0.0, distance_axis, 0.0], key(name))
                .expect("add allowed");
        }
        let lexical = Arc::new(LexicalIndex::new());

        let hits = hybrid_search_within(
            &FixedQueryEmbedding,
            &store,
            &lexical,
            &ScopeFilter::RootOnly,
            "anything",
            2,
            Some(&allow(&["far", "mid", "near"], 5)),
        )
        .await
        .expect("search");

        let names: Vec<_> = hits.iter().map(|hit| hit.qualified_name.clone()).collect();
        assert_eq!(names, [key("near"), key("mid")]);
    }

    /// Drive `rank` against a ranked list of names, recording each window.
    /// `pass` maps a window to the slice of `ranked` a store would return;
    /// `bound` is the store's raw entry count.
    fn rank_within(
        allowed: &CandidateAllowList,
        limit: usize,
        ranked: &[&str],
        pass: impl Fn(usize) -> usize,
        bound: usize,
    ) -> (Vec<String>, Vec<usize>) {
        let mut windows = Vec::new();
        let hits = allowed
            .rank(
                limit,
                |fetch| {
                    windows.push(fetch);
                    Ok(ranked[..pass(fetch).min(ranked.len())]
                        .iter()
                        .map(|name| (key(name), 0.0))
                        .collect())
                },
                || bound,
            )
            .expect("rank");
        let names = hits
            .into_iter()
            .map(|(name, _)| {
                name.rsplit_once('=')
                    .map_or(name.as_str(), |(_, tail)| tail)
                    .to_string()
            })
            .collect();
        (names, windows)
    }

    /// [`rank_within`] over a store whose raw entries are exactly `ranked`.
    fn rank_over(
        allowed: &CandidateAllowList,
        limit: usize,
        ranked: &[&str],
        pass: impl Fn(usize) -> usize,
    ) -> (Vec<String>, Vec<usize>) {
        rank_within(allowed, limit, ranked, pass, ranked.len())
    }

    // The window doubles while allowed hits are short, and stops at the
    // first window covering the raw index.
    #[test]
    fn rank_doubles_the_window_until_the_index_is_exhausted() {
        let ranked: Vec<String> = (0..10).map(|index| format!("other-{index}")).collect();
        let ranked: Vec<&str> = ranked.iter().map(String::as_str).collect();
        let (hits, windows) = rank_over(&allow(&["absent"], 1), 1, &ranked, |fetch| fetch);
        assert!(hits.is_empty());
        assert_eq!(windows, [1, 2, 4, 8, 16]);
    }

    // Regression: widening on every call would re-rank the whole index even
    // when the first window already holds `limit` allowed hits.
    #[test]
    fn rank_stops_at_the_first_window_with_enough_allowed_hits() {
        let ranked = ["a", "x", "b", "y", "z"];
        let (hits, windows) = rank_over(&allow(&["a", "b"], 3), 2, &ranked, |fetch| fetch);
        assert_eq!(hits, ["a", "b"]);
        assert_eq!(windows, [3]);
    }

    // A first window holding some but not `limit` allowed hits widens once;
    // the result is the wider pass's best hits in rank order, no duplicates.
    #[test]
    fn rank_partial_first_window_widens_and_keeps_rank_order() {
        let ranked = ["x", "a", "y", "z", "b", "w"];
        let (hits, windows) = rank_over(&allow(&["a", "b"], 3), 2, &ranked, |fetch| fetch);
        assert_eq!(hits, ["a", "b"]);
        assert_eq!(windows, [3, 6]);
    }

    // Regression: a narrow filter matching fewer memories than `limit`
    // re-ranked every strategy at double width only to confirm the result.
    #[test]
    fn rank_stops_once_every_allowed_name_is_found() {
        let ranked = ["x", "a", "y", "b", "z"];
        let (hits, windows) = rank_within(&allow(&["a", "b"], 8), 5, &ranked, |fetch| fetch, 64);
        assert_eq!(hits, ["a", "b"]);
        assert_eq!(windows, [8]);
    }

    // Fewer allowed hits than `limit` with an allowed name absent from the
    // index: a window covering the raw index returns the partial set
    // without widening.
    #[test]
    fn rank_returns_a_partial_set_from_an_exhausted_index() {
        let ranked = ["x", "keeper"];
        let (hits, windows) =
            rank_over(&allow(&["keeper", "missing"], 1), 2, &ranked, |fetch| fetch);
        assert_eq!(hits, ["keeper"]);
        assert_eq!(windows, [2]);
    }

    #[test]
    fn rank_starts_with_at_least_limit_candidates() {
        let ranked = ["a", "b", "c"];
        let (hits, windows) = rank_over(&allow(&["a", "b", "c"], 0), 3, &ranked, |fetch| fetch);
        assert_eq!(hits, ["a", "b", "c"]);
        assert_eq!(windows, [3]);
    }

    // Regression: a short pass is not proof of exhaustion. usearch drops
    // raw keys that no longer map to a memory, so a pass can come back
    // short while the allowed memory sits further down.
    #[test]
    fn rank_widens_past_a_short_but_unexhausted_pass() {
        let ranked = ["x", "y", "z", "w", "v", "keeper"];
        // Each pass loses one entry to an unmapped key.
        let (hits, windows) = rank_over(&allow(&["keeper"], 2), 1, &ranked, |fetch| fetch - 1);
        assert_eq!(hits, ["keeper"]);
        assert_eq!(windows, [2, 4, 8]);
    }

    // Regression: equal visible counts are not exhaustion either. Four
    // unmapped raw keys outrank `keeper`, so windows 2 and 4 both return
    // nothing; stopping on that plateau misses `keeper` at window 8.
    #[test]
    fn rank_widens_past_a_plateau_of_unmapped_raw_keys() {
        let (hits, windows) = rank_within(
            &allow(&["keeper"], 2),
            1,
            &["keeper"],
            |fetch| usize::from(fetch >= 5),
            5,
        );
        assert_eq!(hits, ["keeper"]);
        assert_eq!(windows, [2, 4, 8]);
    }

    // A fully stale index (every raw key unmapped) still ends the search
    // once the window covers its raw entries.
    #[test]
    fn rank_terminates_on_a_fully_stale_index() {
        let (hits, windows) = rank_within(&allow(&["keeper"], 1), 1, &[], |_| 0, 6);
        assert!(hits.is_empty());
        assert_eq!(windows, [1, 2, 4, 8]);
    }

    // Regression: a store can return several raw keys for one name (an
    // upsert whose old-key removal failed leaves both keys mapped), and
    // fusion dedupes only after `rank` has cut to `limit`. Counting rows,
    // the first window [a, a, a, a] met `limit = 2` and returned [a, a],
    // so fusion surfaced `a` alone and dropped the later allowed `b`.
    #[test]
    fn rank_counts_distinct_names_past_duplicate_keys() {
        let ranked = ["a", "a", "a", "a", "b"];
        let (hits, windows) = rank_over(&allow(&["a", "b"], 4), 2, &ranked, |fetch| fetch);
        assert_eq!(hits, ["a", "b"]);
        assert_eq!(windows, [4, 8]);
    }

    // The window saturates instead of overflowing, and a saturated pass ends
    // the search even if the store reports an unbounded index.
    #[test]
    fn rank_terminates_at_a_saturated_window() {
        let mut windows = Vec::new();
        let hits = allow(&["absent"], usize::MAX / 2 + 1)
            .rank(
                1,
                |fetch| {
                    windows.push(fetch);
                    Ok((0..windows.len())
                        .map(|index| (key(&format!("other-{index}")), 0.0))
                        .collect())
                },
                || usize::MAX,
            )
            .expect("rank");
        assert!(hits.is_empty());
        assert_eq!(windows, [usize::MAX / 2 + 1, usize::MAX]);
    }
}
