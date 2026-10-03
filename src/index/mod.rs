//! Vector index module — defines the `VectorStore` trait and provides
//! concrete implementations backed by usearch (HNSW) and an in-memory
//! HashMap (for tests).

use std::path::Path;

use crate::{
    error::MemoryError,
    types::{Scope, ScopeFilter},
};

/// HashMap-based `VectorStore` implementation for tests.
pub mod in_memory;
/// Usearch HNSW-backed `VectorStore` implementation.
pub mod usearch;

pub use in_memory::InMemoryStore;
pub use usearch::UsearchStore;

// ---------------------------------------------------------------------------
// Sealed trait — prevents external implementations of VectorStore
// ---------------------------------------------------------------------------

pub(crate) mod sealed {
    pub trait Sealed {}
}

// ---------------------------------------------------------------------------
// VectorStore trait
// ---------------------------------------------------------------------------

/// A pluggable vector similarity store.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks and placed behind an `Arc` or `Box`.
///
/// # Object safety
/// The trait is object-safe: `load` (which would return `Self`) is intentionally
/// absent. Each implementation provides its own constructor.
pub trait VectorStore: Send + Sync + sealed::Sealed {
    /// Insert or upsert `vector` for `qualified_name` in the given `scope`.
    ///
    /// Returns the key assigned to the entry in the global "all" index.
    fn add(
        &self,
        scope: &Scope,
        vector: &[f32],
        qualified_name: String,
    ) -> Result<u64, MemoryError>;

    /// Remove the entry for `qualified_name` from `scope` (and from the
    /// all-index). Best-effort — does not fail if the entry is absent.
    fn remove(&self, scope: &Scope, qualified_name: &str) -> Result<(), MemoryError>;

    /// Search for the `limit` nearest neighbours of `query`, filtered by
    /// `filter`.
    ///
    /// Returns `(key, qualified_name, distance)` triples sorted by ascending
    /// distance (lower = more similar).
    fn search(
        &self,
        filter: &ScopeFilter,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(u64, String, f32)>, MemoryError>;

    /// A `limit` at which [`Self::search`] is guaranteed to rank every entry
    /// `filter` can reach, including raw entries it drops before returning
    /// (such as keys that no longer map to a memory). The bound is
    /// conservative, not the smallest sufficient limit. Below it, a pass may
    /// hide candidates even when it returns fewer than `limit`.
    fn search_bound(&self, filter: &ScopeFilter) -> usize;

    /// Look up the vector key for a qualified name in the all-index.
    ///
    /// Returns `None` if the name is not indexed.
    fn find_by_name(&self, qualified_name: &str) -> Option<u64>;

    /// Persist all indexes to subdirectories under `dir`.
    fn save(&self, dir: &Path) -> Result<(), MemoryError>;

    /// Returns `true` when the store is ready to accept queries.
    ///
    /// For `UsearchStore` this is always `true` after construction. For
    /// `InMemoryStore` it returns the configured value (useful to simulate
    /// a not-yet-ready backend in tests).
    fn is_ready(&self) -> bool;

    /// The embedding dimensionality this store was initialised with.
    fn dimensions(&self) -> usize;

    /// The commit SHA last written to or read from the index metadata, if any.
    fn commit_sha(&self) -> Option<String>;

    /// Overwrite the stored commit SHA.
    fn set_commit_sha(&self, sha: Option<&str>);
}

// ---------------------------------------------------------------------------
// Trait-level tests — run against both implementations
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{InMemoryStore, UsearchStore};
    use crate::types::ScopePath;

    fn vec_a() -> Vec<f32> {
        vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    }

    fn vec_b() -> Vec<f32> {
        vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    }

    /// Run all trait-level contract tests against a given `VectorStore`.
    fn check_contract(store: &dyn VectorStore) {
        let scope = Scope::Root;
        let name = "global/contract-test".to_string();

        // TC-02a: add + find_by_name returns Some
        store
            .add(&scope, &vec_a(), name.clone())
            .expect("add should succeed");
        assert!(
            store.find_by_name(&name).is_some(),
            "TC-02a: find_by_name should return Some after add"
        );

        // TC-02a: search returns the added entry
        let results = store
            .search(&ScopeFilter::RootOnly, &vec_a(), 5)
            .expect("search should succeed");
        assert!(
            results.iter().any(|(_, n, _)| n == &name),
            "TC-02a: search should return added entry"
        );

        // TC-02c: upsert replaces old entry — only one result with that name
        store
            .add(&scope, &vec_b(), name.clone())
            .expect("upsert should succeed");
        let results = store
            .search(&ScopeFilter::All, &vec_b(), 10)
            .expect("search after upsert should succeed");
        assert_eq!(
            results.iter().filter(|(_, n, _)| n == &name).count(),
            1,
            "TC-02c: upsert should leave exactly one entry"
        );

        // TC-02b: remove makes entry unreachable
        store.remove(&scope, &name).expect("remove should succeed");
        assert!(
            store.find_by_name(&name).is_none(),
            "TC-02b: find_by_name should return None after remove"
        );
        let results_after = store
            .search(&ScopeFilter::RootOnly, &vec_a(), 5)
            .expect("search after remove should succeed");
        assert!(
            !results_after.iter().any(|(_, n, _)| n == &name),
            "TC-02b: search should not return removed entry"
        );

        // TC-02d: search with Subtree returns correct entries.
        let proj_scope = Scope::Path(ScopePath::new("testproj").unwrap());
        store
            .add(&Scope::Root, &vec_a(), "global/contract-global".to_string())
            .expect("re-add global entry for TC-02d");
        store
            .add(
                &proj_scope,
                &vec_b(),
                "projects/testproj/contract-proj".to_string(),
            )
            .expect("add namespace entry should succeed");
        let pag_results = store
            .search(
                &ScopeFilter::Subtree(ScopePath::new("testproj").unwrap()),
                &vec_a(),
                10,
            )
            .expect("Subtree search should succeed");
        let pag_names: Vec<&str> = pag_results.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(
            pag_names.contains(&"projects/testproj/contract-proj"),
            "TC-02d: Subtree should include namespace entries"
        );
        assert!(
            pag_names.contains(&"global/contract-global"),
            "TC-02d: Subtree should include global entries"
        );
        // Clean up
        store
            .remove(&proj_scope, "projects/testproj/contract-proj")
            .expect("remove namespace entry");
        store
            .remove(&Scope::Root, "global/contract-global")
            .expect("remove global entry");

        // TC-06: is_ready() returns true for a freshly created store.
        assert!(
            store.is_ready(),
            "TC-06: is_ready() should return true for a functioning store"
        );

        // dimensions() returns the value the store was created with.
        assert_eq!(
            store.dimensions(),
            8,
            "dimensions() should return 8 (the value passed to new)"
        );

        // commit_sha / set_commit_sha round-trip.
        assert!(
            store.commit_sha().is_none(),
            "commit_sha() should be None on a fresh store"
        );
        store.set_commit_sha(Some("deadbeef"));
        assert_eq!(
            store.commit_sha(),
            Some("deadbeef".to_string()),
            "commit_sha() should reflect set_commit_sha(Some(...))"
        );
        store.set_commit_sha(None);
        assert!(
            store.commit_sha().is_none(),
            "commit_sha() should be None after set_commit_sha(None)"
        );
    }

    #[test]
    fn trait_contract_usearch_store() {
        let store = UsearchStore::new(8).expect("create UsearchStore");
        check_contract(&store);
    }

    #[test]
    fn trait_contract_in_memory_store() {
        let store = InMemoryStore::new(8);
        check_contract(&store);
    }
}
