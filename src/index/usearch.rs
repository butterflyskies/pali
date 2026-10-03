use std::{
    borrow::Cow,
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, RwLock,
    },
};

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::{
    error::MemoryError,
    health::SubsystemReporter,
    types::{Scope, ScopeFilter, ScopePath},
};

// ---------------------------------------------------------------------------
// RawIndex — private trait abstracting the usearch Index operations
// ---------------------------------------------------------------------------

const INITIAL_INDEX_CAPACITY: usize = 1024;

struct RawSearchResults {
    keys: Vec<u64>,
    distances: Vec<f32>,
}

type RawIndexError = Box<dyn std::error::Error + Send + Sync>;

/// Private trait over the raw vector index operations. Production code uses
/// `UsearchRawIndex`; tests can substitute `FailingRawIndex`.
///
/// Uses `RawIndexError` (not `MemoryError`) so that the error conversion
/// boundary between backend and `VectorIndex` is real and testable.
trait RawIndex: Send + Sync + Sized {
    fn create(dimensions: usize) -> Result<Self, RawIndexError>;
    fn add(&self, key: u64, vector: &[f32]) -> Result<(), RawIndexError>;
    fn remove(&self, key: u64) -> Result<(), RawIndexError>;
    fn search(&self, query: &[f32], count: usize) -> Result<RawSearchResults, RawIndexError>;
    fn save(&self, path: &str) -> Result<(), RawIndexError>;
    fn reserve(&self, capacity: usize) -> Result<(), RawIndexError>;
    fn size(&self) -> usize;
    fn capacity(&self) -> usize;
}

// ---------------------------------------------------------------------------
// UsearchRawIndex — production RawIndex backed by usearch::Index
// ---------------------------------------------------------------------------

struct UsearchRawIndex {
    inner: Index,
}

impl RawIndex for UsearchRawIndex {
    fn create(dimensions: usize) -> Result<Self, RawIndexError> {
        let options = IndexOptions {
            dimensions,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        };
        let inner = Index::new(&options)?;
        inner.reserve(INITIAL_INDEX_CAPACITY)?;
        Ok(Self { inner })
    }

    fn add(&self, key: u64, vector: &[f32]) -> Result<(), RawIndexError> {
        self.inner.add(key, vector).map_err(Into::into)
    }

    fn remove(&self, key: u64) -> Result<(), RawIndexError> {
        self.inner.remove(key).map(|_| ()).map_err(Into::into)
    }

    fn search(&self, query: &[f32], count: usize) -> Result<RawSearchResults, RawIndexError> {
        let m = self.inner.search(query, count)?;
        Ok(RawSearchResults {
            keys: m.keys,
            distances: m.distances,
        })
    }

    fn save(&self, path: &str) -> Result<(), RawIndexError> {
        self.inner.save(path).map_err(Into::into)
    }

    fn reserve(&self, capacity: usize) -> Result<(), RawIndexError> {
        self.inner.reserve(capacity).map_err(Into::into)
    }

    fn size(&self) -> usize {
        self.inner.size()
    }

    fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

// ---------------------------------------------------------------------------
// VectorIndex — generic over RawIndex
// ---------------------------------------------------------------------------

/// Internal state kept behind the mutex.
struct VectorState<R: RawIndex> {
    index: R,
    /// Maps usearch u64 keys → memory name strings.
    key_map: HashMap<u64, String>,
    /// Reverse map: memory name strings → usearch u64 keys (derived from key_map).
    name_map: HashMap<String, u64>,
    /// Monotonic counter used to assign unique vector keys.
    next_key: u64,
    /// Commit SHA at the time this index was last saved/loaded.
    commit_sha: Option<String>,
}

/// Wraps a `RawIndex` implementation and a key-map behind a single `std::sync::Mutex`.
///
/// `VectorIndex<UsearchRawIndex>` is the production path. In tests,
/// `VectorIndex<FailingRawIndex>` enables failure injection.
struct VectorIndex<R: RawIndex = UsearchRawIndex> {
    state: Mutex<VectorState<R>>,
    entry_count: AtomicUsize,
    /// `index.size()` as of the last mutation, so a size read needs no lock.
    /// Refreshed from the index itself while the lock is held, never counted
    /// by hand: the raw index can hold keys the key map does not.
    raw_count: AtomicUsize,
}

/// Lock the mutex, panicking with a consistent message if the lock is poisoned.
///
/// Lock poisoning means a prior thread panicked while holding the lock, leaving
/// the protected data in an unknown state. There is no safe recovery path, so
/// we panic rather than propagate corrupt state to callers.
macro_rules! lock {
    ($mutex:expr) => {
        $mutex
            .lock()
            .expect("VectorIndex state lock poisoned — prior panic corrupted state")
    };
}

impl VectorIndex<UsearchRawIndex> {
    /// Load an existing index from `path`. Also reads `<path>.keys.json`.
    fn load(path: &Path) -> Result<Self, MemoryError> {
        let path_str = path.to_str().ok_or_else(|| MemoryError::InvalidInput {
            reason: "non-UTF-8 index path".to_string(),
        })?;

        // We need to know dimensions to create the IndexOptions for load.
        // usearch::Index::load() restores dimensions from the file, so we
        // use placeholder options here — they are overwritten on load.
        let options = IndexOptions {
            dimensions: 1, // overwritten by load()
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        };
        let inner = Index::new(&options)
            .map_err(|e| MemoryError::Index(format!("init for load: {}", e)))?;
        inner
            .load(path_str)
            .map_err(|e| MemoryError::Index(format!("load: {}", e)))?;

        let KeyMapData {
            key_map,
            next_key,
            commit_sha,
        } = load_key_map(path_str)?;

        let name_map: HashMap<String, u64> = key_map.iter().map(|(&k, v)| (v.clone(), k)).collect();
        if key_map.len() != name_map.len() {
            tracing::warn!(
                key_map_len = key_map.len(),
                name_map_len = name_map.len(),
                "key_map and name_map have different sizes; index may contain duplicate names"
            );
        }

        let count = key_map.len();
        let raw = inner.size();
        Ok(Self {
            state: Mutex::new(VectorState {
                index: UsearchRawIndex { inner },
                key_map,
                name_map,
                next_key,
                commit_sha,
            }),
            entry_count: AtomicUsize::new(count),
            raw_count: AtomicUsize::new(raw),
        })
    }
}

/// Parsed contents of a `<index>.keys.json` file.
struct KeyMapData {
    key_map: HashMap<u64, String>,
    next_key: u64,
    commit_sha: Option<String>,
}

/// Load and deserialise the key map from `<path_str>.keys.json`.
///
/// Supports both the legacy bare-HashMap format and the current
/// `{key_map, next_key, commit_sha}` envelope format.
fn load_key_map(path_str: &str) -> Result<KeyMapData, MemoryError> {
    let keys_path = format!("{}.keys.json", path_str);
    if !std::path::Path::new(&keys_path).exists() {
        return Ok(KeyMapData {
            key_map: HashMap::new(),
            next_key: 0,
            commit_sha: None,
        });
    }

    let json = std::fs::read_to_string(&keys_path)?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| MemoryError::Index(format!("keymap deserialise: {}", e)))?;

    if value.is_object() && value.get("key_map").is_some() {
        // Current format: {key_map, next_key, commit_sha}.
        let key_map: HashMap<u64, String> = serde_json::from_value(value["key_map"].clone())
            .map_err(|e| MemoryError::Index(format!("keymap deserialise: {}", e)))?;
        let next_key = value["next_key"]
            .as_u64()
            .unwrap_or_else(|| key_map.keys().max().map(|k| k + 1).unwrap_or(0));
        let commit_sha = value
            .get("commit_sha")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        Ok(KeyMapData {
            key_map,
            next_key,
            commit_sha,
        })
    } else {
        // Legacy format: bare HashMap.
        let key_map: HashMap<u64, String> = serde_json::from_value(value)
            .map_err(|e| MemoryError::Index(format!("keymap deserialise: {}", e)))?;
        let next_key = key_map.keys().max().map(|k| k + 1).unwrap_or(0);
        Ok(KeyMapData {
            key_map,
            next_key,
            commit_sha: None,
        })
    }
}

/// Convert a `RawIndexError` to `MemoryError::Index`, preserving the message
/// but stripping backend-specific type identity.
fn raw_err(e: RawIndexError) -> MemoryError {
    MemoryError::Index(e.to_string())
}

impl<R: RawIndex> VectorIndex<R> {
    fn new(dimensions: usize) -> Result<Self, MemoryError> {
        let index = R::create(dimensions).map_err(raw_err)?;
        let raw = index.size();
        Ok(Self {
            state: Mutex::new(VectorState {
                index,
                key_map: HashMap::new(),
                name_map: HashMap::new(),
                next_key: 0,
                commit_sha: None,
            }),
            entry_count: AtomicUsize::new(0),
            raw_count: AtomicUsize::new(raw),
        })
    }

    /// Publish the raw index size. Call with the lock held, after any
    /// mutation of `state.index`, on success and failure alike.
    fn sync_raw_count(&self, state: &VectorState<R>) {
        self.raw_count.store(state.index.size(), Ordering::Relaxed);
    }

    fn grow_if_needed_inner(state: &VectorState<R>, additional: usize) -> Result<(), MemoryError> {
        let current_capacity = state.index.capacity();
        let current_size = state.index.size();
        if current_size + additional > current_capacity {
            let new_capacity = (current_capacity + additional).max(current_capacity * 2);
            state.index.reserve(new_capacity).map_err(raw_err)?;
        }
        Ok(())
    }

    /// Find the vector key associated with a qualified memory name.
    fn find_key_by_name(&self, name: &str) -> Option<u64> {
        lock!(self.state).name_map.get(name).copied()
    }

    /// Atomically allocate the next key and add the vector in one lock acquisition.
    /// Returns the assigned key on success. On failure the counter is not advanced.
    fn add_with_next_key(&self, vector: &[f32], name: String) -> Result<u64, MemoryError> {
        let mut state = lock!(self.state);
        Self::grow_if_needed_inner(&state, 1)?;
        let key = state.next_key;
        let added = state.index.add(key, vector);
        self.sync_raw_count(&state);
        added.map_err(raw_err)?;
        state.name_map.insert(name.clone(), key);
        state.key_map.insert(key, name);
        state.next_key = state
            .next_key
            .checked_add(1)
            .expect("vector key space exhausted");
        self.entry_count
            .store(state.key_map.len(), Ordering::Relaxed);
        Ok(key)
    }

    /// Search for the `limit` nearest neighbours of `query`.
    ///
    /// Returns `(key, name, distance)` triples sorted by ascending distance.
    fn search(&self, query: &[f32], limit: usize) -> Result<Vec<(u64, String, f32)>, MemoryError> {
        let state = lock!(self.state);
        let raw = state.index.search(query, limit).map_err(raw_err)?;

        let results = raw
            .keys
            .into_iter()
            .zip(raw.distances)
            .filter_map(|(key, dist)| {
                state
                    .key_map
                    .get(&key)
                    .map(|name| (key, name.clone(), dist))
            })
            .collect();
        Ok(results)
    }

    /// Remove a vector by key.
    fn remove(&self, key: u64) -> Result<(), MemoryError> {
        let mut state = lock!(self.state);
        let removed = state.index.remove(key);
        self.sync_raw_count(&state);
        removed.map_err(raw_err)?;
        if let Some(name) = state.key_map.remove(&key) {
            // Only remove from name_map if it still points to this key.
            // An upsert may have already updated name_map to point to a newer key.
            if state.name_map.get(&name).copied() == Some(key) {
                state.name_map.remove(&name);
            }
            self.entry_count
                .store(state.key_map.len(), Ordering::Relaxed);
        }
        Ok(())
    }

    /// Atomically roll back a failed `add_with_next_key` call.
    ///
    /// Removes `new_key` from the raw index and key_map, then restores
    /// `name_map` to point to `old_key` (or removes the name entry if there
    /// was no prior entry).  All mutations happen under a single lock
    /// acquisition, preventing a partial rollback from leaving inconsistent
    /// state.
    fn rollback_add(&self, new_key: u64, old_key: Option<u64>, name: &str) {
        let mut state = lock!(self.state);
        // Remove the new entry from raw index (best-effort; log on failure).
        if let Err(e) = state.index.remove(new_key) {
            tracing::warn!(error = %e, "rollback: raw index remove failed");
        }
        self.sync_raw_count(&state);
        // Remove the new key from key_map.
        state.key_map.remove(&new_key);
        // Restore name_map to point to the old key (or remove if no old key).
        match old_key {
            Some(old) => state.name_map.insert(name.to_owned(), old),
            None => state.name_map.remove(name),
        };
        self.entry_count
            .store(state.key_map.len(), Ordering::Relaxed);
    }

    /// Return the number of vectors in the raw index, including any whose
    /// key is absent from the key map and so dropped by [`Self::search`].
    fn raw_size(&self) -> usize {
        self.raw_count.load(Ordering::Relaxed)
    }

    /// Return the number of entries currently in the key map.
    fn key_count(&self) -> usize {
        self.entry_count.load(Ordering::Relaxed)
    }

    /// Return the commit SHA stored in the index metadata (if any).
    fn commit_sha(&self) -> Option<String> {
        lock!(self.state).commit_sha.clone()
    }

    /// Set the commit SHA in the index metadata.
    fn set_commit_sha(&self, sha: Option<&str>) {
        lock!(self.state).commit_sha = sha.map(str::to_owned);
    }

    /// Persist the index to `path`. Also writes `<path>.keys.json`.
    fn save(&self, path: &Path) -> Result<(), MemoryError> {
        let path_str = path.to_str().ok_or_else(|| MemoryError::InvalidInput {
            reason: "non-UTF-8 index path".to_string(),
        })?;

        let state = lock!(self.state);
        state.index.save(path_str).map_err(raw_err)?;

        // Persist the key map and counter alongside the index.
        let keys_path = format!("{}.keys.json", path_str);
        let payload = serde_json::json!({
            "key_map": &state.key_map,
            "next_key": state.next_key,
            "commit_sha": state.commit_sha,
        });
        let json = serde_json::to_string(&payload)
            .map_err(|e| MemoryError::Index(format!("keymap serialise: {}", e)))?;
        std::fs::write(&keys_path, json)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ScopeRegistry — typed wrapper around the per-scope index map
// ---------------------------------------------------------------------------

/// Owns all per-scope `VectorIndex` instances.
///
/// Invariant: after `new`, the `Root` scope is always present. New scopes are
/// created on demand by `get_or_create`.
struct ScopeRegistry<R: RawIndex> {
    scopes: HashMap<Scope, VectorIndex<R>>,
}

impl<R: RawIndex> ScopeRegistry<R> {
    fn new(root: VectorIndex<R>) -> Self {
        let mut scopes = HashMap::new();
        scopes.insert(Scope::Root, root);
        Self { scopes }
    }

    fn get(&self, scope: &Scope) -> Option<&VectorIndex<R>> {
        self.scopes.get(scope)
    }

    fn get_or_create(
        &mut self,
        scope: &Scope,
        dimensions: usize,
    ) -> Result<&VectorIndex<R>, MemoryError> {
        // Use the entry API to avoid a redundant lookup after insertion.
        if let std::collections::hash_map::Entry::Vacant(e) = self.scopes.entry(scope.clone()) {
            e.insert(VectorIndex::new(dimensions)?);
        }
        Ok(self
            .scopes
            .get(scope)
            .expect("just inserted or already present"))
    }

    fn insert(&mut self, scope: Scope, index: VectorIndex<R>) {
        self.scopes.insert(scope, index);
    }

    /// Return all indexes whose scope matches `filter`.
    fn matching(&self, filter: &ScopeFilter) -> Vec<(&Scope, &VectorIndex<R>)> {
        self.scopes
            .iter()
            .filter(|(scope, _)| filter.matches(scope))
            .collect()
    }

    fn iter(&self) -> impl Iterator<Item = (&Scope, &VectorIndex<R>)> {
        self.scopes.iter()
    }
}

// ---------------------------------------------------------------------------
// UsearchStore — implements VectorStore backed by VectorIndex<UsearchRawIndex>
// ---------------------------------------------------------------------------

/// Manages multiple `VectorIndex` instances — one per scope (global, each
/// namespace) plus a combined "all" index. Every memory exists in exactly two
/// indexes: its scope-specific index + the "all" index.
///
/// `UsearchStore` is `Send + Sync` because all inner state is protected by
/// `RwLock` / `Mutex`.
#[non_exhaustive]
pub struct UsearchStore {
    inner: UsearchStoreInner<UsearchRawIndex>,
    reporter: SubsystemReporter,
}

/// Generic inner implementation, separated so tests can substitute `R`.
struct UsearchStoreInner<R: RawIndex> {
    /// Per-scope indexes (global + each namespace).
    ///
    /// `RwLock` is used here because searches are the dominant operation and
    /// must not block each other. Writes (add/remove) are serialised with a
    /// write lock to prevent concurrent upserts for the same name from
    /// interleaving. `std::sync::RwLock` is sufficient — no async boundary
    /// crosses these locks.
    scopes: RwLock<ScopeRegistry<R>>,
    /// Combined index containing all vectors.
    all: VectorIndex<R>,
    /// Embedding dimensions (needed to create new scope indexes).
    dimensions: usize,
}

// Locking order: `scopes` (RwLock) is always acquired before any
// `VectorIndex::state` (Mutex). Never hold a VectorIndex Mutex while
// acquiring `scopes`. The `all` index is accessed directly (not through
// `scopes`), but always while `scopes` is already held or after it has
// been released — never in the reverse order.

/// Acquire the scopes `RwLock` for reading, panicking on poison.
macro_rules! scopes_read {
    ($lock:expr) => {
        $lock
            .read()
            .expect("scopes RwLock poisoned — prior panic corrupted state")
    };
}

/// Acquire the scopes `RwLock` for writing, panicking on poison.
macro_rules! scopes_write {
    ($lock:expr) => {
        $lock
            .write()
            .expect("scopes RwLock poisoned — prior panic corrupted state")
    };
}

impl UsearchStore {
    /// Create a new `UsearchStore` with empty global + all indexes.
    ///
    /// `reporter` is called with `report_ok`/`report_err` after each `add` and
    /// `search` so `/readyz` reflects the store's operational state passively.
    pub fn new(dimensions: usize) -> Result<Self, MemoryError> {
        Self::new_with_reporter(dimensions, SubsystemReporter::new())
    }

    /// Create a new `UsearchStore` with a specific health reporter.
    pub fn new_with_reporter(
        dimensions: usize,
        reporter: SubsystemReporter,
    ) -> Result<Self, MemoryError> {
        let global = VectorIndex::new(dimensions)?;
        let all = VectorIndex::new(dimensions)?;
        Ok(Self {
            inner: UsearchStoreInner {
                scopes: RwLock::new(ScopeRegistry::new(global)),
                all,
                dimensions,
            },
            reporter,
        })
    }

    /// Load all indexes from subdirectories under `dir`.
    ///
    /// Missing subdirectories are treated as empty — those scopes will be
    /// rebuilt incrementally on next use.
    pub fn load(dir: &Path, dimensions: usize) -> Result<Self, MemoryError> {
        Self::load_with_reporter(dir, dimensions, SubsystemReporter::new())
    }

    /// Load all indexes from subdirectories under `dir` with a specific health reporter.
    pub fn load_with_reporter(
        dir: &Path,
        dimensions: usize,
        reporter: SubsystemReporter,
    ) -> Result<Self, MemoryError> {
        let span = tracing::info_span!("index.load", key_count = tracing::field::Empty,);
        let _enter = span.enter();

        // If a previous save was interrupted, the on-disk state may be
        // inconsistent (some indexes from current state, others from prior).
        // Rather than loading mixed data, start fresh — indexes are a cache
        // that can always be rebuilt from the source-of-truth markdown files.
        let dirty_marker = dir.join(".save-in-progress");
        if dirty_marker.exists() {
            tracing::warn!("detected interrupted index save — discarding indexes");
            let _ = std::fs::remove_file(&dirty_marker);
            return Self::new_with_reporter(dimensions, reporter);
        }

        // Load all-index.
        let all_path = dir.join("all").join("index.usearch");
        let all = if all_path.exists() {
            VectorIndex::load(&all_path)?
        } else {
            VectorIndex::new(dimensions)?
        };

        // Load global/root index.
        let global_path = dir.join("global").join("index.usearch");
        let global = if global_path.exists() {
            VectorIndex::load(&global_path)?
        } else {
            VectorIndex::new(dimensions)?
        };
        let mut registry = ScopeRegistry::new(global);

        // Scan for namespace indexes under projects/*/ (may be nested).
        let projects_dir = dir.join("projects");
        if projects_dir.is_dir() {
            load_namespace_indexes(&projects_dir, &projects_dir, &mut registry)?;
        }

        let key_count = all.key_count();
        span.record("key_count", key_count);

        Ok(Self {
            inner: UsearchStoreInner {
                scopes: RwLock::new(registry),
                all,
                dimensions,
            },
            reporter,
        })
    }
}

/// Recursively load namespace scope indexes from `dir`, building scope paths
/// relative to `namespaces_root`.
///
/// A directory that contains an `index.usearch` file is loaded as a scope index.
/// Subdirectories are recursed to support hierarchical paths like `org/team`.
/// If a directory resolves to an invalid scope path, it is skipped entirely —
/// its children are not visited, since they would inherit the invalid prefix.
fn load_namespace_indexes(
    dir: &Path,
    namespaces_root: &Path,
    registry: &mut ScopeRegistry<UsearchRawIndex>,
) -> Result<(), MemoryError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MemoryError::Index(format!("read namespaces dir: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| MemoryError::Index(format!("read dir entry: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let scope = match ScopePath::from_dir(&path, namespaces_root) {
            Ok(sp) => sp,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping namespace index with invalid scope path"
                );
                // Don't recurse: children inherit this invalid prefix and will
                // also fail, so pruning here avoids pointless work.
                continue;
            }
        };

        let index_path = path.join("index.usearch");
        if index_path.exists() {
            registry.insert(Scope::Path(scope), VectorIndex::load(&index_path)?);
        }

        load_namespace_indexes(&path, namespaces_root, registry)?;
    }
    Ok(())
}

// Shared logic for both production and test-generic paths.
impl<R: RawIndex> UsearchStoreInner<R> {
    fn add(
        &self,
        scope: &Scope,
        vector: &[f32],
        qualified_name: String,
    ) -> Result<u64, MemoryError> {
        let dimensions = vector.len();
        let span = tracing::debug_span!(
            "index.add",
            scope = %scope.dir_prefix(),
            dimensions,
            key_count = tracing::field::Empty,
        );
        let _enter = span.enter();

        if vector.len() != self.dimensions {
            return Err(MemoryError::InvalidInput {
                reason: format!(
                    "expected {} dimensions, got {}",
                    self.dimensions,
                    vector.len()
                ),
            });
        }

        // Write lock serialises the full find→insert→remove composite so
        // concurrent upserts for the same name cannot interleave. Reads
        // (via `search`) use a read lock and are not blocked by other reads.
        let mut scopes = scopes_write!(self.scopes);

        let scope_idx = scopes.get_or_create(scope, self.dimensions)?;

        // Capture old keys before inserting new ones.
        let old_scope_key = scope_idx.find_key_by_name(&qualified_name);
        let old_all_key = self.all.find_key_by_name(&qualified_name);

        // Insert into scope index first.
        let new_scope_key = scope_idx.add_with_next_key(vector, qualified_name.clone())?;

        // Insert into all-index; if this fails, roll back scope insert atomically,
        // restoring name_map to its pre-add state (so the old entry remains reachable).
        let all_key = match self.all.add_with_next_key(vector, qualified_name.clone()) {
            Ok(key) => key,
            Err(e) => {
                scope_idx.rollback_add(new_scope_key, old_scope_key, &qualified_name);
                return Err(e);
            }
        };

        // Both succeeded — now clean up old entries.
        if let Some(key) = old_scope_key {
            let _ = scope_idx.remove(key);
        }
        if let Some(key) = old_all_key {
            let _ = self.all.remove(key);
        }

        // Record key_count (all-index size) after insertion.
        span.record("key_count", self.all.key_count());

        Ok(all_key)
    }

    fn remove(&self, scope: &Scope, qualified_name: &str) -> Result<(), MemoryError> {
        let _span = tracing::debug_span!(
            "index.remove",
            scope = %scope.dir_prefix(),
        )
        .entered();

        // Write lock serialises with concurrent adds for the same name.
        let scopes = scopes_write!(self.scopes);

        // Remove from scope index (best-effort).
        if let Some(scope_idx) = scopes.get(scope) {
            if let Some(key) = scope_idx.find_key_by_name(qualified_name) {
                if let Err(e) = scope_idx.remove(key) {
                    tracing::warn!(
                        qualified_name = %qualified_name,
                        error = %e,
                        "scope index removal failed; continuing to all-index"
                    );
                }
            }
        }

        // Remove from all-index (best-effort).
        if let Some(key) = self.all.find_key_by_name(qualified_name) {
            if let Err(e) = self.all.remove(key) {
                tracing::warn!(
                    qualified_name = %qualified_name,
                    error = %e,
                    "all-index removal failed"
                );
            }
        }

        Ok(())
    }

    fn search(
        &self,
        filter: &ScopeFilter,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(u64, String, f32)>, MemoryError> {
        let dimensions = query.len();
        let scope_str: Cow<'_, str> = match filter {
            ScopeFilter::RootOnly => "global".into(),
            ScopeFilter::All => "all".into(),
            ScopeFilter::Subtree(p) => format!("subtree:{p}").into(),
        };
        let span = tracing::debug_span!(
            "index.search",
            scope = %scope_str,
            dimensions,
            key_count = self.all.key_count(),
            count = tracing::field::Empty,
        );
        let _enter = span.enter();

        if query.len() != self.dimensions {
            return Err(MemoryError::InvalidInput {
                reason: format!(
                    "expected {} dimensions, got {}",
                    self.dimensions,
                    query.len()
                ),
            });
        }

        let results = match filter {
            ScopeFilter::All => self.all.search(query, limit),

            ScopeFilter::RootOnly => {
                let scopes = scopes_read!(self.scopes);
                match scopes.get(&Scope::Root) {
                    Some(root_idx) => root_idx.search(query, limit),
                    None => Ok(Vec::new()),
                }
            }

            ScopeFilter::Subtree(_) => {
                let scopes = scopes_read!(self.scopes);
                let mut combined = Vec::new();
                for (_, idx) in scopes.matching(filter) {
                    combined.extend(idx.search(query, limit)?);
                }

                // Deduplicate by qualified name, sort by ascending distance, top-k.
                let mut seen = std::collections::HashSet::new();
                combined.retain(|(_, name, _)| seen.insert(name.clone()));
                combined.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
                combined.truncate(limit);
                Ok(combined)
            }
        };
        if let Ok(ref r) = results {
            span.record("count", r.len());
        }
        results
    }

    /// Raw vectors a search under `filter` can rank. Unmapped raw keys count,
    /// since they take window slots before `search` drops them. Subtree
    /// searches merge several scope indexes and cut to `limit`, so their
    /// bound is the sum of those indexes' sizes.
    fn search_bound(&self, filter: &ScopeFilter) -> usize {
        match filter {
            ScopeFilter::All => self.all.raw_size(),
            ScopeFilter::RootOnly => scopes_read!(self.scopes)
                .get(&Scope::Root)
                .map_or(0, VectorIndex::raw_size),
            ScopeFilter::Subtree(_) => scopes_read!(self.scopes)
                .matching(filter)
                .into_iter()
                .fold(0, |sum, (_, idx)| sum.saturating_add(idx.raw_size())),
        }
    }

    fn find_key_by_name(&self, qualified_name: &str) -> Option<u64> {
        self.all.find_key_by_name(qualified_name)
    }

    fn save(&self, dir: &Path) -> Result<(), MemoryError> {
        let span = tracing::debug_span!("index.save", key_count = tracing::field::Empty,);
        let _enter = span.enter();

        std::fs::create_dir_all(dir)?;

        // Write a dirty marker — if we crash mid-save, the next load will see
        // this and ignore commit SHAs (forcing a fresh rebuild).
        let marker = dir.join(".save-in-progress");
        std::fs::write(&marker, b"")?;

        let result = self.save_inner(dir, &span);

        // Always remove the marker — a transient I/O failure should not
        // force a full reindex on next startup.
        let _ = std::fs::remove_file(&marker);

        result
    }

    fn save_inner(&self, dir: &Path, span: &tracing::Span) -> Result<(), MemoryError> {
        // Acquire scopes read lock before accessing any indexes.
        let scopes = scopes_read!(self.scopes);

        // Persist all-index.
        let all_dir = dir.join("all");
        std::fs::create_dir_all(&all_dir)?;
        self.all.save(&all_dir.join("index.usearch"))?;

        // Persist per-scope indexes.
        for (scope, idx) in scopes.iter() {
            let scope_dir = dir.join(scope.dir_prefix().as_ref());
            std::fs::create_dir_all(&scope_dir)?;
            idx.save(&scope_dir.join("index.usearch"))?;
        }

        // Record total key count (all-index is authoritative — it holds every entry).
        span.record("key_count", self.all.key_count());

        // scopes lock dropped at end of function.
        Ok(())
    }

    fn commit_sha(&self) -> Option<String> {
        self.all.commit_sha()
    }

    fn set_commit_sha(&self, sha: Option<&str>) {
        let scopes = scopes_read!(self.scopes);
        self.all.set_commit_sha(sha);
        for (_, idx) in scopes.iter() {
            idx.set_commit_sha(sha);
        }
    }
}

// ---------------------------------------------------------------------------
// VectorStore implementation for UsearchStore
// ---------------------------------------------------------------------------

impl crate::index::sealed::Sealed for UsearchStore {}

impl crate::index::VectorStore for UsearchStore {
    fn add(
        &self,
        scope: &Scope,
        vector: &[f32],
        qualified_name: String,
    ) -> Result<u64, MemoryError> {
        let result = self.inner.add(scope, vector, qualified_name);
        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("index add failed"),
        }
        result
    }

    fn remove(&self, scope: &Scope, qualified_name: &str) -> Result<(), MemoryError> {
        let result = self.inner.remove(scope, qualified_name);
        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("index remove failed"),
        }
        result
    }

    fn search(
        &self,
        filter: &ScopeFilter,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(u64, String, f32)>, MemoryError> {
        let result = self.inner.search(filter, query, limit);
        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("index search failed"),
        }
        result
    }

    fn search_bound(&self, filter: &ScopeFilter) -> usize {
        self.inner.search_bound(filter)
    }

    fn find_by_name(&self, qualified_name: &str) -> Option<u64> {
        self.inner.find_key_by_name(qualified_name)
    }

    fn save(&self, dir: &Path) -> Result<(), MemoryError> {
        self.inner.save(dir)
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions
    }

    fn commit_sha(&self) -> Option<String> {
        self.inner.commit_sha()
    }

    fn set_commit_sha(&self, sha: Option<&str>) {
        self.inner.set_commit_sha(sha)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::VectorStore;

    // -----------------------------------------------------------------------
    // FailingRawIndex — test double for error injection
    // -----------------------------------------------------------------------

    /// Controls which operation `FailingRawIndex` will fail on.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum FailOn {
        Add,
        Remove,
        /// Apply the add to the inner index, then report failure: a backend
        /// that is not failure-atomic.
        AddAfterMutating,
        /// Apply the remove to the inner index, then report failure.
        RemoveAfterMutating,
        Search,
        Save,
        Reserve,
        None,
    }

    /// A `RawIndex` implementation that fails on the configured operation.
    struct FailingRawIndex {
        /// Underlying real index for operations that should succeed.
        inner: Index,
        /// Which operation to fail.
        fail_on: FailOn,
        /// Counter for tracking calls (for partial-failure scenarios).
        call_count: Mutex<usize>,
        /// Fail after this many successful calls (0 = always fail).
        fail_after: usize,
    }

    impl FailingRawIndex {
        fn new(dimensions: usize, fail_on: FailOn, fail_after: usize) -> Self {
            let options = IndexOptions {
                dimensions,
                metric: MetricKind::Cos,
                quantization: ScalarKind::F32,
                ..Default::default()
            };
            let inner = Index::new(&options).expect("create failing index");
            inner.reserve(1024).expect("reserve");
            Self {
                inner,
                fail_on,
                call_count: Mutex::new(0),
                fail_after,
            }
        }

        fn should_fail(&self, op: FailOn) -> bool {
            if self.fail_on != op {
                return false;
            }
            let mut count = self.call_count.lock().unwrap();
            if self.fail_after == 0 || *count >= self.fail_after {
                return true;
            }
            *count += 1;
            false
        }

        fn injected_error(op: &str) -> RawIndexError {
            format!("injected {op} failure").into()
        }
    }

    impl RawIndex for FailingRawIndex {
        fn create(dimensions: usize) -> Result<Self, RawIndexError> {
            Ok(FailingRawIndex::new(dimensions, FailOn::None, 0))
        }

        fn add(&self, key: u64, vector: &[f32]) -> Result<(), RawIndexError> {
            if self.should_fail(FailOn::Add) {
                return Err(Self::injected_error("add"));
            }
            if self.should_fail(FailOn::AddAfterMutating) {
                self.inner.add(key, vector)?;
                return Err(Self::injected_error("add"));
            }
            self.inner.add(key, vector).map_err(Into::into)
        }

        fn remove(&self, key: u64) -> Result<(), RawIndexError> {
            if self.should_fail(FailOn::Remove) {
                return Err(Self::injected_error("remove"));
            }
            if self.should_fail(FailOn::RemoveAfterMutating) {
                self.inner.remove(key)?;
                return Err(Self::injected_error("remove"));
            }
            self.inner.remove(key).map(|_| ()).map_err(Into::into)
        }

        fn search(&self, query: &[f32], count: usize) -> Result<RawSearchResults, RawIndexError> {
            if self.should_fail(FailOn::Search) {
                return Err(Self::injected_error("search"));
            }
            let m = self.inner.search(query, count)?;
            Ok(RawSearchResults {
                keys: m.keys,
                distances: m.distances,
            })
        }

        fn save(&self, path: &str) -> Result<(), RawIndexError> {
            if self.should_fail(FailOn::Save) {
                return Err(Self::injected_error("save"));
            }
            self.inner.save(path).map_err(Into::into)
        }

        fn reserve(&self, capacity: usize) -> Result<(), RawIndexError> {
            if self.should_fail(FailOn::Reserve) {
                return Err(Self::injected_error("reserve"));
            }
            self.inner.reserve(capacity).map_err(Into::into)
        }

        fn size(&self) -> usize {
            self.inner.size()
        }

        fn capacity(&self) -> usize {
            self.inner.capacity()
        }
    }

    // Helper to build a VectorIndex<FailingRawIndex> directly.
    fn make_failing_index(
        dimensions: usize,
        fail_on: FailOn,
        fail_after: usize,
    ) -> VectorIndex<FailingRawIndex> {
        VectorIndex {
            state: Mutex::new(VectorState {
                index: FailingRawIndex::new(dimensions, fail_on, fail_after),
                key_map: HashMap::new(),
                name_map: HashMap::new(),
                next_key: 0,
                commit_sha: None,
            }),
            entry_count: AtomicUsize::new(0),
            raw_count: AtomicUsize::new(0),
        }
    }

    // A test-only variant of UsearchStoreInner that works over FailingRawIndex.
    // We need this to test rollback.
    struct FailableStore {
        inner: UsearchStoreInner<FailingRawIndex>,
    }

    fn make_failable_store(
        dimensions: usize,
        all_fail_on: FailOn,
        all_fail_after: usize,
    ) -> FailableStore {
        let all = make_failing_index(dimensions, all_fail_on, all_fail_after);
        let scope = make_failing_index(dimensions, FailOn::None, 0);
        FailableStore {
            inner: UsearchStoreInner {
                scopes: RwLock::new(ScopeRegistry::new(scope)),
                all,
                dimensions,
            },
        }
    }

    // -----------------------------------------------------------------------
    // VectorIndex tests (low-level)
    // -----------------------------------------------------------------------

    fn make_index() -> VectorIndex {
        VectorIndex::new(4).expect("failed to create index")
    }

    fn dummy_vec() -> Vec<f32> {
        vec![1.0, 0.0, 0.0, 0.0]
    }

    /// Verify that `remove(old_key)` does NOT clobber `name_map` when an
    /// upsert has already updated `name_map` to point to a newer key.
    #[test]
    fn remove_old_key_does_not_clobber_upserted_name_map_entry() {
        let index = make_index();
        let v = dummy_vec();

        let old_key = index
            .add_with_next_key(&v, "global/foo".to_string())
            .expect("first add failed");
        let new_key = index
            .add_with_next_key(&v, "global/foo".to_string())
            .expect("second add failed");

        assert_ne!(old_key, new_key, "keys must differ");

        index.remove(old_key).expect("remove failed");

        assert_eq!(
            index.find_key_by_name("global/foo"),
            Some(new_key),
            "name_map entry for new_key was incorrectly removed"
        );
    }

    #[test]
    fn remove_only_key_clears_name_map() {
        let index = make_index();
        let v = dummy_vec();

        let key = index
            .add_with_next_key(&v, "global/bar".to_string())
            .expect("add failed");

        index.remove(key).expect("remove failed");

        assert_eq!(
            index.find_key_by_name("global/bar"),
            None,
            "name_map entry should have been cleared"
        );
    }

    // -----------------------------------------------------------------------
    // UsearchStore tests — via VectorStore trait
    // -----------------------------------------------------------------------

    fn make_store() -> UsearchStore {
        UsearchStore::new(8).expect("failed to create UsearchStore")
    }

    fn vec_a() -> Vec<f32> {
        vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    }

    fn vec_b() -> Vec<f32> {
        vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    }

    fn vec_c() -> Vec<f32> {
        vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    }

    #[test]
    fn usearch_store_add_inserts_into_scope_and_all() {
        let si: &dyn VectorStore = &make_store();
        let scope = Scope::Root;
        let name = "global/memory-a".to_string();

        si.add(&scope, &vec_a(), name.clone()).expect("add failed");

        assert!(si.find_by_name(&name).is_some(), "should be in all-index");

        let results = si
            .search(&ScopeFilter::RootOnly, &vec_a(), 5)
            .expect("search failed");
        assert!(
            results.iter().any(|(_, n, _)| n == &name),
            "should be found in global search"
        );
    }

    #[test]
    fn usearch_store_remove_removes_from_both() {
        let si: &dyn VectorStore = &make_store();
        let scope = Scope::Root;
        let name = "global/memory-rm".to_string();

        si.add(&scope, &vec_a(), name.clone()).expect("add failed");
        assert!(si.find_by_name(&name).is_some(), "should exist");

        si.remove(&scope, &name).expect("remove failed");

        assert!(
            si.find_by_name(&name).is_none(),
            "should be gone from all-index"
        );

        let results = si
            .search(&ScopeFilter::RootOnly, &vec_a(), 5)
            .expect("search failed");
        assert!(
            !results.iter().any(|(_, n, _)| n == &name),
            "should not appear in global search after removal"
        );
    }

    #[test]
    fn usearch_store_search_root_only() {
        let si: &dyn VectorStore = &make_store();
        let proj = Scope::Path(ScopePath::new("myproj").unwrap());

        si.add(&Scope::Root, &vec_a(), "global/mem-global".to_string())
            .expect("add global failed");
        si.add(&proj, &vec_b(), "projects/myproj/mem-proj".to_string())
            .expect("add namespace failed");

        let results = si
            .search(&ScopeFilter::RootOnly, &vec_a(), 5)
            .expect("search failed");

        let names: Vec<&str> = results.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"global/mem-global"),
            "should contain global"
        );
        assert!(
            !names.contains(&"projects/myproj/mem-proj"),
            "should NOT contain namespace memory"
        );
    }

    #[test]
    fn usearch_store_search_subtree() {
        let si: &dyn VectorStore = &make_store();
        let proj_a = Scope::Path(ScopePath::new("alpha").unwrap());
        let proj_b = Scope::Path(ScopePath::new("beta").unwrap());

        si.add(&Scope::Root, &vec_a(), "global/g1".to_string())
            .expect("add global failed");
        si.add(&proj_a, &vec_b(), "projects/alpha/a1".to_string())
            .expect("add alpha failed");
        si.add(&proj_b, &vec_c(), "projects/beta/b1".to_string())
            .expect("add beta failed");

        let results = si
            .search(
                &ScopeFilter::Subtree(ScopePath::new("alpha").unwrap()),
                &vec_a(),
                10,
            )
            .expect("search failed");

        let names: Vec<&str> = results.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"global/g1"), "should contain global");
        assert!(names.contains(&"projects/alpha/a1"), "should contain alpha");
        assert!(
            !names.contains(&"projects/beta/b1"),
            "should NOT contain beta"
        );
    }

    #[test]
    fn usearch_store_search_all() {
        let si: &dyn VectorStore = &make_store();
        let proj = Scope::Path(ScopePath::new("foo").unwrap());

        si.add(&Scope::Root, &vec_a(), "global/x".to_string())
            .expect("add global");
        si.add(&proj, &vec_b(), "projects/foo/y".to_string())
            .expect("add namespace");

        let results = si
            .search(&ScopeFilter::All, &vec_a(), 10)
            .expect("search failed");

        let names: Vec<&str> = results.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"global/x"), "all should include global");
        assert!(
            names.contains(&"projects/foo/y"),
            "all should include namespace"
        );
    }

    #[test]
    fn usearch_store_upsert_replaces_old_entry() {
        let si: &dyn VectorStore = &make_store();
        let name = "global/memo".to_string();
        si.add(&Scope::Root, &vec_a(), name.clone()).unwrap();
        si.add(&Scope::Root, &vec_b(), name.clone()).unwrap();
        let results = si.search(&ScopeFilter::All, &vec_b(), 10).unwrap();
        assert_eq!(
            results.iter().filter(|(_, n, _)| n == &name).count(),
            1,
            "upsert should leave exactly one entry for the name"
        );
    }

    #[test]
    fn usearch_store_dirty_marker_discards_indexes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let si = UsearchStore::new(8).expect("create");
        let store: &dyn VectorStore = &si;
        store
            .add(&Scope::Root, &vec_a(), "global/test-mem".to_string())
            .expect("add");
        store.set_commit_sha(Some("abc123"));
        store.save(dir.path()).expect("save");

        std::fs::write(dir.path().join(".save-in-progress"), b"").unwrap();

        let loaded = UsearchStore::load(dir.path(), 8).expect("load");
        let loaded: &dyn VectorStore = &loaded;
        assert!(
            loaded.commit_sha().is_none(),
            "dirty marker should result in no SHA"
        );
        assert!(
            loaded.find_by_name("global/test-mem").is_none(),
            "dirty marker should discard all indexed data"
        );
        assert!(
            !dir.path().join(".save-in-progress").exists(),
            "marker should be cleaned up"
        );
    }

    #[test]
    fn usearch_store_save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let si = UsearchStore::new(8).expect("create");
        let store: &dyn VectorStore = &si;
        let proj = Scope::Path(ScopePath::new("rtrip").unwrap());

        store
            .add(&Scope::Root, &vec_a(), "global/rt-global".to_string())
            .expect("add global");
        store
            .add(&proj, &vec_b(), "projects/rtrip/rt-proj".to_string())
            .expect("add namespace");

        store.save(dir.path()).expect("save failed");

        let loaded = UsearchStore::load(dir.path(), 8).expect("load failed");
        let loaded: &dyn VectorStore = &loaded;

        assert!(
            loaded.find_by_name("global/rt-global").is_some(),
            "global memory should survive round-trip"
        );
        assert!(
            loaded.find_by_name("projects/rtrip/rt-proj").is_some(),
            "namespace memory should survive round-trip"
        );

        let results = loaded
            .search(
                &ScopeFilter::Subtree(ScopePath::new("rtrip").unwrap()),
                &vec_a(),
                10,
            )
            .expect("search failed");
        let names: Vec<&str> = results.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"global/rt-global"));
        assert!(names.contains(&"projects/rtrip/rt-proj"));
    }

    #[test]
    fn usearch_store_same_short_name_different_scopes_coexist() {
        let si: &dyn VectorStore = &make_store();
        si.add(&Scope::Root, &vec_a(), "global/foo".to_string())
            .unwrap();
        si.add(
            &Scope::Path(ScopePath::new("p").unwrap()),
            &vec_b(),
            "projects/p/foo".to_string(),
        )
        .unwrap();
        assert!(si.find_by_name("global/foo").is_some());
        assert!(si.find_by_name("projects/p/foo").is_some());
        assert_ne!(
            si.find_by_name("global/foo"),
            si.find_by_name("projects/p/foo"),
            "different scopes should have distinct keys"
        );
    }

    // -----------------------------------------------------------------------
    // TC-03: FailingRawIndex is injectable
    // -----------------------------------------------------------------------

    #[test]
    fn tc03_failing_raw_index_is_injectable() {
        let idx = make_failing_index(4, FailOn::Add, 0);
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let result = idx.add_with_next_key(&v, "test/name".to_string());
        assert!(
            result.is_err(),
            "FailingRawIndex with FailOn::Add should return error"
        );
    }

    // -----------------------------------------------------------------------
    // TC-04a: Rollback on all-index failure — scope index entry count unchanged
    // -----------------------------------------------------------------------

    #[test]
    fn tc04a_rollback_on_all_index_failure_scope_count_unchanged() {
        // Build a store where the all-index fails immediately.
        let fs = make_failable_store(8, FailOn::Add, 0);

        let scope = Scope::Root;
        let name = "global/rollback-test".to_string();

        // This add should fail because all-index.add fails.
        let result = fs.inner.add(&scope, &vec_a(), name.clone());
        assert!(result.is_err(), "add should fail when all-index fails");

        // Scope index should be empty — rollback removed the entry.
        let scopes = fs.inner.scopes.read().unwrap();
        let scope_idx = scopes.get(&scope).expect("global scope must exist");
        assert_eq!(
            scope_idx.key_count(),
            0,
            "TC-04a: scope index entry count should be 0 after rollback (was: {})",
            scope_idx.key_count()
        );
    }

    // -----------------------------------------------------------------------
    // TC-04b: Rollback doesn't corrupt existing entries
    // -----------------------------------------------------------------------

    #[test]
    fn tc04b_rollback_does_not_corrupt_existing_entries() {
        // all-index: first add succeeds, second fails.
        let fs = make_failable_store(8, FailOn::Add, 1);

        let scope = Scope::Root;

        // First add should succeed (fail_after=1 means first call succeeds).
        let first_name = "global/existing".to_string();
        fs.inner
            .add(&scope, &vec_a(), first_name.clone())
            .expect("first add should succeed");

        // Second add should fail (all-index now fails).
        let second_name = "global/failing".to_string();
        let result = fs.inner.add(&scope, &vec_b(), second_name.clone());
        assert!(result.is_err(), "second add should fail");

        // The first entry must still be findable.
        assert!(
            fs.inner.find_key_by_name(&first_name).is_some(),
            "TC-04b: existing entry should not be corrupted by rollback"
        );
        assert!(
            fs.inner.find_key_by_name(&second_name).is_none(),
            "TC-04b: failed entry should not be in the index"
        );
    }

    // -----------------------------------------------------------------------
    // TC-04c: Upsert rollback — original entry survives when all-index fails
    // -----------------------------------------------------------------------

    #[test]
    fn tc04c_upsert_rollback_preserves_original_entry() {
        // Seed one successful entry, then re-add the same name with the
        // all-index configured to fail on the second call.  The original
        // entry must still be reachable via find_key_by_name afterwards.
        let fs = make_failable_store(8, FailOn::Add, 1);

        let scope = Scope::Root;
        let name = "global/upsert-rollback".to_string();

        // First add: both scope and all-index succeed (fail_after=1).
        let original_key = fs
            .inner
            .add(&scope, &vec_a(), name.clone())
            .expect("TC-04c: first add should succeed");

        // Second add (upsert): all-index fails on this call → rollback.
        let result = fs.inner.add(&scope, &vec_b(), name.clone());
        assert!(
            result.is_err(),
            "TC-04c: second add should fail when all-index fails"
        );

        // The original entry must still be reachable.
        let key_after = fs.inner.find_key_by_name(&name);
        assert_eq!(
            key_after,
            Some(original_key),
            "TC-04c: original entry must survive upsert rollback (expected key {original_key}, got {key_after:?})"
        );
    }

    // -----------------------------------------------------------------------
    // TC-05a: Errors are MemoryError variants
    // -----------------------------------------------------------------------

    #[test]
    fn tc05a_errors_are_memory_error_variants() {
        let idx = make_failing_index(4, FailOn::Add, 0);
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let err = idx
            .add_with_next_key(&v, "test/name".to_string())
            .unwrap_err();
        // Should be MemoryError::Index variant.
        assert!(
            matches!(err, MemoryError::Index(_)),
            "TC-05a: error should be MemoryError::Index, got: {:?}",
            err
        );
    }

    // -----------------------------------------------------------------------
    // TC-05b: Error Display has no usearch internals
    // -----------------------------------------------------------------------

    #[test]
    fn tc05b_error_display_has_no_raw_usearch_type_names() {
        let idx = make_failing_index(4, FailOn::Add, 0);
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let err = idx
            .add_with_next_key(&v, "test/name".to_string())
            .unwrap_err();
        let display = format!("{}", err);
        // The display should contain our wrapper prefix, not raw usearch types.
        assert!(
            display.contains("index error"),
            "TC-05b: display should contain 'index error', got: {}",
            display
        );
        assert!(
            !display.contains("usearch") && !display.contains("cxx::Exception"),
            "TC-05b: display must not leak raw backend type names, got: {}",
            display
        );
    }

    // -----------------------------------------------------------------------
    // TC-05b (real): Dimension mismatch produces clean MemoryError
    // -----------------------------------------------------------------------

    #[test]
    fn tc05b_dimension_mismatch_error_is_clean() {
        let store = UsearchStore::new(8).expect("create");
        let wrong_dims = vec![1.0_f32, 0.0, 0.0]; // 3 dims, store expects 8
        let err = store
            .inner
            .add(&Scope::Root, &wrong_dims, "global/bad-dims".to_string())
            .unwrap_err();
        let display = format!("{}", err);
        assert!(
            !display.contains("usearch") && !display.contains("cxx::Exception"),
            "error display must not leak backend type names, got: {}",
            display
        );
        assert!(
            matches!(err, MemoryError::InvalidInput { .. }),
            "dimension mismatch should return InvalidInput, got: {:?}",
            err
        );
    }

    // -----------------------------------------------------------------------
    // TC-06a: UsearchStore::is_ready() returns true
    // -----------------------------------------------------------------------

    #[test]
    fn tc06a_usearch_store_is_ready() {
        let store = UsearchStore::new(4).expect("create");
        assert!(
            store.is_ready(),
            "TC-06a: UsearchStore::is_ready() should return true"
        );
    }

    // -----------------------------------------------------------------------
    // Tag pre-filter: raw keys absent from the key map (#370)
    // -----------------------------------------------------------------------

    mod prefilter {
        use std::{collections::HashSet, sync::Arc};

        use super::*;
        use crate::{
            embedding::EmbeddingBackend,
            search::{hybrid_search_within, CandidateAllowList, LexicalIndex},
            types::{MemoryName, MemoryRef},
        };

        const QUERY: [f32; 4] = [1.0, 0.0, 0.0, 0.0];

        struct FixedQuery;

        #[async_trait::async_trait]
        impl EmbeddingBackend for FixedQuery {
            async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
                Ok(texts.iter().map(|_| QUERY.to_vec()).collect())
            }

            fn dimensions(&self) -> usize {
                QUERY.len()
            }
        }

        fn key(name: &str) -> String {
            MemoryRef::new(Scope::Root, MemoryName::new(name.to_string()).unwrap()).qualified_path()
        }

        /// Add `count` raw vectors next to the query to the all-index with no
        /// key-map entry, the shape a key map that lost entries its raw index
        /// still holds leaves behind. `search` drops these keys.
        fn add_unmapped(store: &UsearchStore, count: u64) {
            let state = lock!(store.inner.all.state);
            VectorIndex::grow_if_needed_inner(&state, count as usize).expect("grow");
            for offset in 0..count {
                let vector = [1.0, 0.001 * offset as f32, 0.0, 0.0];
                state
                    .index
                    .add(1_000_000 + offset, &vector)
                    .expect("add unmapped raw key");
            }
            store.inner.all.sync_raw_count(&state);
        }

        async fn recall(
            store: &UsearchStore,
            allowed: &[&str],
            fetch: usize,
            limit: usize,
        ) -> Vec<String> {
            let names: HashSet<String> = allowed.iter().map(|name| key(name)).collect();
            let hits = hybrid_search_within(
                &FixedQuery,
                store,
                &Arc::new(LexicalIndex::new()),
                &ScopeFilter::All,
                "no lexical hit",
                limit,
                Some(&CandidateAllowList::new(names, fetch)),
            )
            .await
            .expect("search");
            hits.into_iter().map(|hit| hit.qualified_name).collect()
        }

        // Regression: four unmapped raw keys outrank the only allowed memory.
        // Recall's first window is in-scope count + limit = 2, so windows 2
        // and 4 both come back empty after the key map drops the raw keys.
        // Equal visible counts are not exhaustion: window 8 reaches `keeper`.
        #[tokio::test]
        async fn widens_past_a_plateau_of_unmapped_raw_keys() {
            let store = UsearchStore::new(QUERY.len()).expect("create");
            store
                .add(&Scope::Root, &[0.0, 1.0, 0.0, 0.0], key("keeper"))
                .expect("add keeper");
            add_unmapped(&store, 4);

            assert_eq!(recall(&store, &["keeper"], 2, 1).await, [key("keeper")]);
        }

        /// Add `count` raw vectors next to the query to the all-index, each
        /// mapped to `name`: the shape an upsert leaves when its best-effort
        /// removal of the old key fails, so old and new keys both map to the
        /// same memory. `search` returns every one of them.
        fn add_duplicates(store: &UsearchStore, name: &str, count: u64) {
            let mut state = lock!(store.inner.all.state);
            VectorIndex::grow_if_needed_inner(&state, count as usize).expect("grow");
            for offset in 0..count {
                let raw_key = 2_000_000 + offset;
                let vector = [1.0, 0.001 * offset as f32, 0.0, 0.0];
                state
                    .index
                    .add(raw_key, &vector)
                    .expect("add duplicate raw key");
                state.key_map.insert(raw_key, key(name));
            }
            store.inner.all.sync_raw_count(&state);
        }

        // Regression: three stale keys still mapped to `a` outrank its live
        // key, so recall's first window (2 in-scope + limit 2 = 4) holds
        // [a, a, a, a]. Counting rows met `limit` there and cut to [a, a];
        // fusion deduped that to [a] and silently dropped the allowed `b`.
        #[tokio::test]
        async fn counts_distinct_names_past_duplicate_mapped_keys() {
            let store = UsearchStore::new(QUERY.len()).expect("create");
            store
                .add(&Scope::Root, &[1.0, 0.1, 0.0, 0.0], key("a"))
                .expect("add a");
            store
                .add(&Scope::Root, &[1.0, 0.5, 0.0, 0.0], key("b"))
                .expect("add b");
            add_duplicates(&store, "a", 3);

            assert_eq!(
                recall(&store, &["a", "b"], 4, 2).await,
                [key("a"), key("b")]
            );
        }

        // Widening still ends on an index whose raw keys are all stale and
        // where no allowed memory is indexed.
        #[tokio::test]
        async fn terminates_on_a_fully_stale_index() {
            let store = UsearchStore::new(QUERY.len()).expect("create");
            add_unmapped(&store, 6);

            assert!(recall(&store, &["keeper"], 1, 1).await.is_empty());
        }

        // A size read must not wait on the index mutex: `search_bound` runs on
        // every widening round of a tag-filtered recall, alongside writers.
        #[test]
        fn raw_size_does_not_take_the_index_lock() {
            let index = std::sync::Arc::new(VectorIndex::<UsearchRawIndex>::new(4).unwrap());
            index
                .add_with_next_key(&[1.0, 0.0, 0.0, 0.0], "a".to_owned())
                .unwrap();
            let guard = lock!(index.state);
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::sync::Arc::clone(&index);
            std::thread::spawn(move || {
                let _ = tx.send(reader.raw_size());
            });
            let size = rx.recv_timeout(std::time::Duration::from_secs(2));
            drop(guard);
            assert_eq!(size, Ok(1), "raw_size blocked on the held index lock");
        }

        // The published size tracks `index.size()` through every mutation path,
        // including a failed add and a rollback, where the key map and the raw
        // index can disagree.
        #[test]
        fn raw_size_tracks_the_index_through_failures_and_rollback() {
            let check = |idx: &VectorIndex<FailingRawIndex>| {
                assert_eq!(idx.raw_size(), lock!(idx.state).index.size());
            };
            let idx = make_failing_index(4, FailOn::Add, 2);
            let k0 = idx
                .add_with_next_key(&[1.0, 0.0, 0.0, 0.0], "a".to_owned())
                .unwrap();
            check(&idx);
            let k1 = idx
                .add_with_next_key(&[0.0, 1.0, 0.0, 0.0], "b".to_owned())
                .unwrap();
            check(&idx);
            assert!(idx
                .add_with_next_key(&[0.0, 0.0, 1.0, 0.0], "c".to_owned())
                .is_err());
            check(&idx);
            idx.rollback_add(k1, None, "b");
            check(&idx);
            idx.remove(k0).unwrap();
            check(&idx);
            assert_eq!(idx.raw_size(), 0);
        }

        // usearch does not promise that a failed add or remove leaves the
        // index untouched. When the backend mutates and then errors, the
        // published size must still follow the index, which only the
        // error-path refresh guarantees.
        #[test]
        fn raw_size_tracks_a_backend_that_mutates_then_fails() {
            let check = |idx: &VectorIndex<FailingRawIndex>| {
                assert_eq!(idx.raw_size(), lock!(idx.state).index.size());
            };
            let adds = make_failing_index(4, FailOn::AddAfterMutating, 1);
            adds.add_with_next_key(&[1.0, 0.0, 0.0, 0.0], "a".to_owned())
                .unwrap();
            assert!(adds
                .add_with_next_key(&[0.0, 1.0, 0.0, 0.0], "b".to_owned())
                .is_err());
            check(&adds);
            assert_eq!(adds.raw_size(), 2, "the failed add still landed");

            let removes = make_failing_index(4, FailOn::RemoveAfterMutating, 0);
            removes
                .add_with_next_key(&[1.0, 0.0, 0.0, 0.0], "a".to_owned())
                .unwrap();
            let key = removes.find_key_by_name("a").expect("key");
            assert!(removes.remove(key).is_err());
            check(&removes);
            assert_eq!(removes.raw_size(), 0, "the failed remove still landed");
        }

        // The bound counts raw keys the key map lost, and a subtree bound
        // sums its scope indexes: a subtree search cuts the merged hits to
        // `limit`, so the largest single index would truncate real hits.
        #[test]
        fn search_bound_counts_unmapped_keys_and_sums_subtree_indexes() {
            let store = UsearchStore::new(QUERY.len()).expect("create");
            let scope = |path: &str| Scope::Path(ScopePath::new(path).expect("scope"));
            for (scope, name) in [
                (Scope::Root, "root"),
                (scope("org/a"), "a1"),
                (scope("org/a"), "a2"),
                (scope("org/b"), "b1"),
                (scope("other"), "o1"),
            ] {
                let qualified =
                    MemoryRef::new(scope.clone(), MemoryName::new(name.to_string()).unwrap())
                        .qualified_path();
                store.add(&scope, &QUERY, qualified).expect("add");
            }
            add_unmapped(&store, 3);

            assert_eq!(store.search_bound(&ScopeFilter::All), 8);
            assert_eq!(store.search_bound(&ScopeFilter::RootOnly), 1);
            let org = ScopeFilter::Subtree(ScopePath::new("org").expect("scope"));
            assert_eq!(store.search_bound(&org), 4);
        }
    }
}
