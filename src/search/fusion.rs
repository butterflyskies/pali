//! Reciprocal rank fusion (RRF) for merging semantic and lexical results.
//!
//! RRF combines ranked lists without needing the underlying scores to be
//! comparable: each list contributes `1 / (K + rank)` per document, so a
//! top-ranked lexical hit can surface even when its embedding distance is
//! poor (and vice versa). The constant `K = 60` is the standard value from
//! Cormack et al. (2009) — it damps the influence of lower ranks without
//! letting any single list dominate.

use std::collections::HashMap;

/// Standard RRF damping constant.
const RRF_K: f64 = 60.0;

/// A single result produced by [`reciprocal_rank_fusion`].
#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    /// Canonical index key (`v1:scope=<scope>;name=<name>`).
    pub qualified_name: String,
    /// Cosine distance from the semantic strategy, if it returned this hit.
    pub semantic_distance: Option<f32>,
    /// BM25 score from the lexical strategy, if it returned this hit.
    pub lexical_score: Option<f32>,
    /// Combined RRF score (higher is better).
    pub score: f64,
}

impl FusedHit {
    /// Which strategies contributed this hit: `"both"`, `"semantic"`, or
    /// `"lexical"`.
    pub fn match_type(&self) -> &'static str {
        match (self.semantic_distance, self.lexical_score) {
            (Some(_), Some(_)) => "both",
            (Some(_), None) => "semantic",
            (None, _) => "lexical",
        }
    }
}

/// Which retrieval strategy produced a [`RankedList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchSource {
    /// Embedding similarity; hit values are cosine distances (ascending).
    Semantic,
    /// BM25 keyword search; hit values are BM25 scores (descending).
    Lexical,
}

/// One hit in a [`RankedList`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankedHit {
    /// Canonical index key (`v1:scope=<scope>;name=<name>`).
    pub(crate) qualified_name: String,
    /// The strategy's own measure for the hit (see [`MatchSource`]).
    pub(crate) value: f32,
}

impl RankedHit {
    pub(crate) fn new(qualified_name: String, value: f32) -> Self {
        Self {
            qualified_name,
            value,
        }
    }
}

/// One retrieval strategy's hits for a query, best first.
///
/// Fusion uses only the list positions; each hit's value is carried
/// through to the [`FusedHit`] fields.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankedList {
    /// The strategy that produced the hits.
    pub(crate) source: MatchSource,
    /// Hits, best first.
    pub(crate) hits: Vec<RankedHit>,
}

impl RankedList {
    /// Build a list from `(qualified_name, value)` pairs, best first.
    pub(crate) fn from_pairs(source: MatchSource, pairs: Vec<(String, f32)>) -> Self {
        Self {
            source,
            hits: pairs
                .into_iter()
                .map(|(qualified_name, value)| RankedHit::new(qualified_name, value))
                .collect(),
        }
    }
}

/// Merge a semantic result list (ascending distance) and a lexical result
/// list (descending BM25 score) into a single ranking via reciprocal rank
/// fusion.
///
/// Both inputs must already be sorted best-first; ranks are taken from list
/// positions. Duplicate names within one list keep their best (first) rank.
/// The output is sorted by descending fused score. Score ties break
/// lexical-first (a hit with a lexical contribution outranks a tied hit
/// without one — so the top lexical hit is never displaced by a tied
/// semantic-only hit), then by ascending qualified name, so the ordering is
/// fully deterministic. At most `limit` hits are returned.
pub fn reciprocal_rank_fusion(
    semantic: &[(String, f32)],
    lexical: &[(String, f32)],
    limit: usize,
) -> Vec<FusedHit> {
    fuse(
        &[
            RankedList::from_pairs(MatchSource::Semantic, semantic.to_vec()),
            RankedList::from_pairs(MatchSource::Lexical, lexical.to_vec()),
        ],
        limit,
    )
}

/// Merge ranked lists into a single ranking via reciprocal rank fusion.
///
/// Each list contributes `1 / (K + rank + 1)` to every hit it contains and
/// fills that hit's field for its [`MatchSource`]. A name that appears more
/// than once in a list keeps its first (best) rank and value. At most one
/// list may carry each source: a second list of the same source would only
/// half-merge (its ranks would count for names the first list lacked and be
/// dropped for the rest), so this is checked with a `debug_assert!`.
/// Ordering and tie-breaking are as documented on
/// [`reciprocal_rank_fusion`]. At most `limit` hits are returned.
pub(crate) fn fuse(lists: &[RankedList], limit: usize) -> Vec<FusedHit> {
    debug_assert!(
        lists.iter().enumerate().all(|(i, list)| lists[..i]
            .iter()
            .all(|earlier| earlier.source != list.source)),
        "fuse() takes at most one list per source"
    );

    let mut merged: HashMap<&str, FusedHit> = HashMap::new();

    for list in lists {
        for (
            rank,
            RankedHit {
                qualified_name,
                value,
            },
        ) in list.hits.iter().enumerate()
        {
            let hit = merged
                .entry(qualified_name.as_str())
                .or_insert_with(|| FusedHit {
                    qualified_name: qualified_name.clone(),
                    semantic_distance: None,
                    lexical_score: None,
                    score: 0.0,
                });
            let slot = match list.source {
                MatchSource::Semantic => &mut hit.semantic_distance,
                MatchSource::Lexical => &mut hit.lexical_score,
            };
            if slot.is_none() {
                *slot = Some(*value);
                hit.score += rrf_contribution(rank);
            }
        }
    }

    let mut hits: Vec<FusedHit> = merged.into_values().collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            // Lexical wins score ties: the only structural tie is lexical
            // rank r vs semantic rank r (both contribute 1/(K+r+1)), and an
            // exact keyword match must not be displaced by it (#55).
            .then_with(|| b.lexical_score.is_some().cmp(&a.lexical_score.is_some()))
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
    });
    hits.truncate(limit);
    hits
}

/// RRF contribution of a zero-based rank: `1 / (K + rank + 1)`.
fn rrf_contribution(rank: usize) -> f64 {
    // Precision loss is irrelevant here: ranks are small (≤ recall limit).
    #[allow(clippy::cast_precision_loss)]
    let rank = rank as f64;
    1.0 / (RRF_K + rank + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(hits: &[FusedHit]) -> Vec<&str> {
        hits.iter().map(|h| h.qualified_name.as_str()).collect()
    }

    fn sem(entries: &[(&str, f32)]) -> Vec<(String, f32)> {
        entries.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn agreement_between_lists_ranks_first() {
        let semantic = sem(&[("a", 0.1), ("b", 0.2), ("c", 0.3)]);
        let lexical = sem(&[("b", 9.0), ("d", 5.0)]);

        let hits = reciprocal_rank_fusion(&semantic, &lexical, 10);

        // "b" appears in both lists, so it outranks the single-list hits.
        assert_eq!(names(&hits)[0], "b");
        assert_eq!(hits[0].match_type(), "both");
        assert_eq!(hits[0].semantic_distance, Some(0.2));
        assert_eq!(hits[0].lexical_score, Some(9.0));
    }

    #[test]
    fn top_lexical_hit_surfaces_without_semantic_support() {
        // "needle" is absent from the semantic list entirely (the
        // "happy birthday" failure mode) but is the top lexical hit.
        let semantic = sem(&[("decoy1", 0.3), ("decoy2", 0.35), ("decoy3", 0.4)]);
        let lexical = sem(&[("needle", 12.0)]);

        let hits = reciprocal_rank_fusion(&semantic, &lexical, 10);

        // Lexical rank 0 ties semantic rank 0 on RRF score, and the
        // lexical-first tie-break resolves it: "needle" must rank first,
        // ahead of every semantic-only candidate.
        assert_eq!(
            names(&hits)[0],
            "needle",
            "top lexical hit must outrank all semantic-only hits"
        );
        assert_eq!(hits[0].match_type(), "lexical");
        assert_eq!(hits[0].semantic_distance, None);
    }

    #[test]
    fn equal_score_ties_prefer_lexical_regardless_of_name() {
        // Semantic rank 0 and lexical rank 0 contribute identical RRF
        // scores; the lexical hit must win the tie even when its name
        // sorts after the semantic hit's.
        let semantic = sem(&[("aaa", 0.1)]);
        let lexical = sem(&[("zzz", 3.0)]);

        let hits = reciprocal_rank_fusion(&semantic, &lexical, 10);

        assert_eq!(names(&hits), vec!["zzz", "aaa"]);
        assert_eq!(hits[0].match_type(), "lexical");
    }

    #[test]
    fn output_is_truncated_to_limit() {
        let semantic = sem(&[("a", 0.1), ("b", 0.2), ("c", 0.3)]);
        let lexical = sem(&[("d", 3.0), ("e", 2.0)]);

        let hits = reciprocal_rank_fusion(&semantic, &lexical, 2);

        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn duplicate_names_within_a_list_keep_best_rank() {
        let semantic = sem(&[("a", 0.1), ("a", 0.9)]);
        let lexical = sem(&[("a", 5.0), ("a", 1.0)]);

        let hits = reciprocal_rank_fusion(&semantic, &lexical, 10);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].semantic_distance, Some(0.1));
        assert_eq!(hits[0].lexical_score, Some(5.0));
        let expected = 2.0 / (RRF_K + 1.0);
        assert!((hits[0].score - expected).abs() < 1e-12);
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert!(reciprocal_rank_fusion(&[], &[], 10).is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "at most one list per source")]
    fn fuse_rejects_two_lists_with_the_same_source() {
        let list = || RankedList::from_pairs(MatchSource::Semantic, sem(&[("a", 0.1)]));
        let _ = fuse(&[list(), list()], 10);
    }

    #[test]
    fn fusion_is_deterministic() {
        let semantic = sem(&[("a", 0.1), ("b", 0.2), ("c", 0.3)]);
        let lexical = sem(&[("c", 4.0), ("d", 3.0), ("a", 2.0)]);

        let first = reciprocal_rank_fusion(&semantic, &lexical, 10);
        for _ in 0..10 {
            assert_eq!(first, reciprocal_rank_fusion(&semantic, &lexical, 10));
        }
    }
}
