//! BM25 keyword search over memory names and content, backed by Tantivy.
//!
//! The lexical index is held entirely in RAM and rebuilt from the git repo
//! on startup (indexing text is cheap, unlike embedding it), so there is no
//! on-disk index to version or migrate. Writes go through the same handlers
//! that update the vector index, keeping the two views consistent.
//!
//! Mutations are batched: [`LexicalIndex::apply`] takes a list of
//! [`LexicalOp`]s and performs exactly one Tantivy commit and one reader
//! reload for the whole batch, and [`LexicalIndex::apply_async`] runs that
//! blocking work on the Tokio blocking pool so async workers are never
//! stalled behind a commit.
//!
//! # Failure/repair contract (#310, ADR-0039)
//!
//! Git is authoritative; this index is derived state. Any failed mutation
//! rolls back its uncommitted ops (so a later batch can never commit them)
//! and marks the index **degraded** — [`LexicalIndex::is_degraded`] — after
//! which [`LexicalIndex::search`] errors and hybrid recall falls back to
//! semantic-only. Repair is a rebuild from git truth: capture a token with
//! [`LexicalIndex::begin_rebuild`] *before* listing the repo, then call
//! [`LexicalIndex::rebuild_from`]; the token detects failures and concurrent
//! mirrors that raced the rebuild, keeping the index degraded until a
//! quiescent rebuild converges. Cancellation is a non-event:
//! [`LexicalIndex::apply_async`] dispatches its batch to the blocking pool
//! *eagerly*, so a caller dropping the future (request cancellation) never
//! strands a half-mirrored git write — the batch still applies, or fails and
//! flags degraded.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use tantivy::{
    collector::TopDocs,
    query::{BooleanQuery, Occur, PhraseQuery, Query, QueryParser},
    schema::{Field, Schema, Value, STORED, STRING, TEXT},
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term,
};
use tracing::{debug, warn};

use crate::{
    error::MemoryError,
    repo::traced_spawn_blocking,
    types::{parse_qualified_name, ScopeFilter},
};

/// Heap budget for the single Tantivy writer thread. Tantivy requires at
/// least 15 MB; the corpus (markdown memories) is far smaller than this.
const WRITER_HEAP_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on candidates collected per query before scope filtering.
/// Scope filtering happens post-search, so a filtered query must be able to
/// look past out-of-scope matches; this caps the worst case.
const MAX_CANDIDATES: u64 = 10_000;

/// One memory's searchable text, as fed to [`LexicalIndex::rebuild`].
#[derive(Debug, Clone)]
pub struct LexicalDoc {
    /// Canonical index key (`v1:scope=<scope>;name=<name>`).
    pub qualified_name: String,
    /// Bare memory name (searchable).
    pub name: String,
    /// Full memory content (searchable).
    pub content: String,
}

/// A single mutation applied by [`LexicalIndex::apply`].
///
/// Ops within one batch are applied in order (Tantivy serialises them by
/// opstamp), so a `Remove` followed by an `Upsert` of the same key yields
/// a fresh document, not a ghost.
#[derive(Debug, Clone)]
pub enum LexicalOp {
    /// Insert or replace the document for the contained key.
    Upsert(LexicalDoc),
    /// Delete the document with this qualified name. No-op if absent.
    Remove(String),
}

/// In-RAM BM25 index over memory names and content.
///
/// Thread-safe: reads go through a Tantivy [`IndexReader`], writes are
/// serialised by an internal mutex. Each *batch* of writes commits and
/// reloads the reader once so results are immediately visible — acceptable
/// because writes happen at memory-mutation frequency, not query frequency.
/// Callers on the async runtime must use [`LexicalIndex::apply_async`],
/// which moves the commit onto the blocking pool.
///
/// Construction is infallible: if the underlying Tantivy index cannot be
/// initialised (practically impossible for a RAM directory), the instance
/// is *disabled* — every operation returns an error, which callers already
/// treat as best-effort degradation to semantic-only retrieval.
pub struct LexicalIndex {
    inner: Option<Inner>,
    /// Count of divergence events (failed or interrupted mutations). The
    /// index is consistent with git truth only when a rebuild has observed
    /// every event so far: `clean_epoch == dirty_epoch`.
    dirty_epoch: AtomicU64,
    /// The `dirty_epoch` value captured by the most recent successful
    /// rebuild, *before* its repo listing. Always `<= dirty_epoch`.
    clean_epoch: AtomicU64,
    /// Count of successfully committed `apply` batches. Used by
    /// [`Self::rebuild_from`] to detect mirrors that raced a rebuild's repo
    /// listing (the rebuild would silently drop them otherwise).
    applied_batches: AtomicU64,
    /// Single-flight guard for background repair rebuilds.
    repairing: AtomicBool,
    /// Test-only failure injection: the next matching operation stage fails
    /// with an error taking exactly the code path a real Tantivy error would.
    #[cfg(test)]
    fail_next: Mutex<Option<FailPoint>>,
}

/// Explicit lifecycle signal for the lexical index, consumed by ADR-0039's
/// status surface (health endpoints, lifecycle probes).
///
/// [`LexicalIndex::is_degraded`] and [`LexicalIndex::is_repairing`] alone
/// cannot distinguish a *disabled* index (construction failed, every
/// operation errors, repair can never recover it) from a healthy one — both
/// report `false`/`false`. This enum removes that ambiguity: lifecycle
/// consumers branch on [`LexicalIndex::status`] instead of inferring state
/// from error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalStatus {
    /// Construction failed; keyword search is unavailable for the process
    /// lifetime. Rebuilds cannot recover this state (maps to the lifecycle
    /// doc's `Failed`).
    Failed,
    /// Consistent with git truth as of the last converged rebuild.
    Available,
    /// Diverged from git truth; a rebuild is required but not running.
    Degraded,
    /// Diverged from git truth and a repair rebuild currently holds the
    /// single-flight slot (maps to the lifecycle doc's `Rebuilding`).
    Repairing,
}

/// Snapshot handed out by [`LexicalIndex::begin_rebuild`] and consumed by
/// [`LexicalIndex::rebuild_from`].
///
/// Must be captured **before** listing the repo: it records which divergence
/// events and committed mirrors the rebuild's document set can possibly
/// reflect, so anything that happens after the capture keeps (or puts) the
/// index in the degraded state instead of being silently lost.
#[derive(Debug, Clone, Copy)]
pub struct RebuildToken {
    dirty: u64,
    applied: u64,
}

/// Test-only injection point inside a lexical mutation.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailPoint {
    /// Fail before applying the op (or rebuild doc) at this zero-based
    /// position — earlier ops have already been queued on the writer.
    BeforeOp(usize),
    /// Fail the commit: the whole batch is queued but not durably committed.
    Commit,
    /// Fail the reader reload: the commit has already succeeded.
    Reload,
    /// Panic inside `apply` (before the writer lock is taken), simulating a
    /// worker that dies at an unknown point instead of erroring cleanly.
    Panic,
}

/// Marks the lexical index rebuild-required when dropped without being
/// defused.
///
/// This is the cancellation-independent supervision primitive for detached
/// mutation work (#314, ADR-0039): the guard lives *inside* the detached
/// task or blocking worker, so a panic at an unknown point — or the runtime
/// dropping the task at shutdown — records the divergence even when no
/// request future survives to observe a `JoinError`. Normal completion
/// (`Ok` *or* `Err`, both of which handle their own bookkeeping) defuses it.
pub(crate) struct DegradeOnDrop {
    index: Option<Arc<LexicalIndex>>,
    reason: &'static str,
}

impl DegradeOnDrop {
    /// Arm a guard that flags `index` with `reason` unless defused.
    pub(crate) fn new(index: Arc<LexicalIndex>, reason: &'static str) -> Self {
        Self {
            index: Some(index),
            reason,
        }
    }

    /// Disarm the guard: the protected section completed normally.
    pub(crate) fn defuse(mut self) {
        self.index = None;
    }
}

impl Drop for DegradeOnDrop {
    fn drop(&mut self) {
        if let Some(index) = self.index.take() {
            index.mark_rebuild_required(self.reason);
        }
    }
}

struct Inner {
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    field_qualified_name: Field,
    field_name: Field,
    field_content: Field,
}

impl Inner {
    fn build() -> Result<Self, MemoryError> {
        let mut schema_builder = Schema::builder();
        // Raw (untokenised) unique key — used for deletes and result lookup.
        let field_qualified_name = schema_builder.add_text_field("qualified_name", STRING | STORED);
        // Tokenised searchable fields (default tokenizer: simple + lowercase).
        let field_name = schema_builder.add_text_field("name", TEXT);
        let field_content = schema_builder.add_text_field("content", TEXT);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let writer = index
            .writer_with_num_threads(1, WRITER_HEAP_BYTES)
            .map_err(lexical_error)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(lexical_error)?;

        Ok(Self {
            index,
            writer: Mutex::new(writer),
            reader,
            field_qualified_name,
            field_name,
            field_content,
        })
    }

    fn lock_writer(&self) -> Result<std::sync::MutexGuard<'_, IndexWriter>, MemoryError> {
        self.writer
            .lock()
            .map_err(|_| MemoryError::Index("lexical index writer lock poisoned".to_string()))
    }
}

impl Default for LexicalIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LexicalIndex {
    /// Create an empty in-RAM lexical index.
    ///
    /// Infallible by design: an initialisation failure produces a disabled
    /// instance whose operations all error, degrading recall to
    /// semantic-only rather than preventing startup.
    pub fn new() -> Self {
        let inner = match Inner::build() {
            Ok(inner) => Some(inner),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "lexical index initialisation failed — keyword search disabled"
                );
                None
            }
        };
        Self {
            inner,
            dirty_epoch: AtomicU64::new(0),
            clean_epoch: AtomicU64::new(0),
            applied_batches: AtomicU64::new(0),
            repairing: AtomicBool::new(false),
            #[cfg(test)]
            fail_next: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            inner: None,
            dirty_epoch: AtomicU64::new(0),
            clean_epoch: AtomicU64::new(0),
            applied_batches: AtomicU64::new(0),
            repairing: AtomicBool::new(false),
            fail_next: Mutex::new(None),
        }
    }

    /// Whether the index has diverged from git truth: a mutation failed or
    /// was interrupted and no rebuild has covered it yet.
    ///
    /// While degraded, [`Self::search`] errors and hybrid recall serves
    /// semantic-only results; repair is a rebuild from git truth
    /// ([`Self::begin_rebuild`] + [`Self::rebuild_from`]).
    ///
    /// A freshly created (empty) index is *not* degraded: unbuilt-at-boot is
    /// a lifecycle state (Booting/SemanticReady), not a divergence event.
    pub fn is_degraded(&self) -> bool {
        self.clean_epoch.load(Ordering::Acquire) < self.dirty_epoch.load(Ordering::Acquire)
    }

    /// Record a divergence event: the index can no longer be proven
    /// consistent with git truth and must be rebuilt from it.
    ///
    /// Idempotent in effect (extra calls just require the next rebuild to be
    /// more recent). Public so external coordinators (lifecycle management,
    /// operational tooling) can force a rebuild-required state.
    pub fn mark_rebuild_required(&self, reason: &str) {
        let epoch = self.dirty_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        warn!(
            reason,
            epoch, "lexical index degraded — rebuild from git truth required"
        );
    }

    /// Capture the rebuild token. Must be called **before** listing the repo
    /// for the document set passed to [`Self::rebuild_from`].
    pub fn begin_rebuild(&self) -> RebuildToken {
        RebuildToken {
            dirty: self.dirty_epoch.load(Ordering::Acquire),
            applied: self.applied_batches.load(Ordering::Acquire),
        }
    }

    /// Try to claim the single-flight repair slot. Returns `true` if the
    /// caller now owns it and must call [`Self::finish_repair`] when done.
    pub fn try_claim_repair(&self) -> bool {
        self.repairing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release the repair slot claimed with [`Self::try_claim_repair`].
    pub fn finish_repair(&self) {
        self.repairing.store(false, Ordering::Release);
    }

    /// Whether a repair rebuild currently holds the single-flight slot.
    /// Together with [`Self::is_degraded`] this distinguishes the
    /// `Rebuilding` lifecycle state from `Degraded` (rebuild required but
    /// not yet running).
    pub fn is_repairing(&self) -> bool {
        self.repairing.load(Ordering::Acquire)
    }

    /// Whether the underlying Tantivy index was constructed at all.
    ///
    /// `false` means construction failed and the instance is permanently
    /// disabled: every operation errors and no rebuild can recover it.
    /// This is a distinct axis from [`Self::is_degraded`] — a disabled
    /// index may report `is_degraded() == false` simply because nothing
    /// has recorded a divergence event yet.
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }

    /// Explicit lifecycle status for ADR-0039's status surface.
    ///
    /// Precedence: a disabled index is always [`LexicalStatus::Failed`],
    /// regardless of the epoch counters (a disabled index also accumulates
    /// divergence events, but repair can never recover it). Otherwise the
    /// degraded/repairing flags map exactly onto the lifecycle contract:
    /// degraded + repairing ⇒ `Repairing`, degraded ⇒ `Degraded`,
    /// else ⇒ `Available`.
    pub fn status(&self) -> LexicalStatus {
        if !self.is_available() {
            LexicalStatus::Failed
        } else if self.is_degraded() {
            if self.is_repairing() {
                LexicalStatus::Repairing
            } else {
                LexicalStatus::Degraded
            }
        } else {
            LexicalStatus::Available
        }
    }

    /// Arm a one-shot injected failure for the next matching mutation stage.
    #[cfg(test)]
    pub(crate) fn fail_next(&self, point: FailPoint) {
        *self.fail_next.lock().expect("failpoint lock") = Some(point);
    }

    /// Consume the armed failpoint if it matches `point`, erroring exactly
    /// where a real Tantivy failure would.
    #[cfg(test)]
    fn check_failpoint(&self, point: FailPoint) -> Result<(), MemoryError> {
        let mut armed = self.fail_next.lock().expect("failpoint lock");
        if *armed == Some(point) {
            *armed = None;
            return Err(MemoryError::Index(format!(
                "lexical index: injected failure at {point:?}"
            )));
        }
        Ok(())
    }

    /// Consume an armed [`FailPoint::Panic`] and panic, simulating a worker
    /// that dies mid-mutation rather than erroring cleanly. The failpoint
    /// lock is released first so the unwind does not poison it.
    #[cfg(test)]
    fn check_panic_failpoint(&self) {
        let mut armed = self.fail_next.lock().expect("failpoint lock");
        if *armed == Some(FailPoint::Panic) {
            *armed = None;
            drop(armed);
            panic!("lexical index: injected panic in apply");
        }
    }

    fn inner(&self) -> Result<&Inner, MemoryError> {
        self.inner.as_ref().ok_or_else(|| {
            MemoryError::Index("lexical index unavailable (initialisation failed)".to_string())
        })
    }

    /// Apply a batch of mutations with exactly one commit and one reader
    /// reload, regardless of batch size.
    ///
    /// Failure contract: on any error the batch's queued-but-uncommitted ops
    /// are rolled back (a later batch can never commit them) and the index
    /// is marked degraded ([`Self::is_degraded`]) until a rebuild from git
    /// truth repairs it. An `Err` therefore always means "index flagged, no
    /// partial state left behind".
    ///
    /// Blocking: takes the writer mutex and performs a Tantivy commit.
    /// Callers on the async runtime must use [`Self::apply_async`] instead.
    pub fn apply(&self, ops: Vec<LexicalOp>) -> Result<(), MemoryError> {
        let span = tracing::debug_span!("lexical.apply", ops = ops.len());
        let _guard = span.enter();

        if ops.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        self.check_panic_failpoint();
        let inner = match self.inner() {
            Ok(inner) => inner,
            Err(e) => {
                self.mark_rebuild_required("lexical index unavailable");
                return Err(e);
            }
        };
        let mut writer = match inner.lock_writer() {
            Ok(writer) => writer,
            Err(e) => {
                self.mark_rebuild_required("writer lock poisoned");
                return Err(e);
            }
        };

        let queued = (|| {
            // The index is only consumed by the test-only failpoint seam.
            #[allow(clippy::unused_enumerate_index)]
            for (_i, op) in ops.iter().enumerate() {
                #[cfg(test)]
                self.check_failpoint(FailPoint::BeforeOp(_i))?;
                match op {
                    LexicalOp::Upsert(item) => {
                        writer.delete_term(Term::from_field_text(
                            inner.field_qualified_name,
                            &item.qualified_name,
                        ));
                        let mut doc = TantivyDocument::default();
                        doc.add_text(inner.field_qualified_name, &item.qualified_name);
                        doc.add_text(inner.field_name, &item.name);
                        doc.add_text(inner.field_content, &item.content);
                        writer.add_document(doc).map_err(lexical_error)?;
                    }
                    LexicalOp::Remove(qualified_name) => {
                        writer.delete_term(Term::from_field_text(
                            inner.field_qualified_name,
                            qualified_name,
                        ));
                    }
                }
            }
            #[cfg(test)]
            self.check_failpoint(FailPoint::Commit)?;
            writer.commit().map_err(lexical_error)?;
            Ok(())
        })();

        if let Err(e) = queued {
            // Discard the batch's uncommitted ops so a later successful
            // batch can never ghost-commit them.
            if let Err(rb) = writer.rollback() {
                warn!(error = %rb, "lexical writer rollback failed after batch error");
            }
            drop(writer);
            self.mark_rebuild_required("mutation batch failed");
            return Err(e);
        }

        // Record the committed mirror while still holding the writer lock,
        // so a concurrent rebuild (serialised on the same lock) can never
        // observe the commit without the counter.
        self.applied_batches.fetch_add(1, Ordering::AcqRel);
        drop(writer);

        #[cfg(test)]
        if let Err(e) = self.check_failpoint(FailPoint::Reload) {
            self.mark_rebuild_required("reader reload failed");
            return Err(e);
        }
        inner.reader.reload().map_err(|e| {
            // The commit is durable but the reader view is stale; treat the
            // reported failure as divergence so repair restores certainty.
            self.mark_rebuild_required("reader reload failed");
            lexical_error(e)
        })
    }

    /// Run [`Self::apply`] on the Tokio blocking pool.
    ///
    /// This is the mutation entry point for async handlers: the writer
    /// lock, the Tantivy commit, and the reader reload never run on an
    /// async worker thread.
    ///
    /// Cancellation contract: the batch is dispatched to the blocking pool
    /// **eagerly**, before the returned future is first polled. Dropping the
    /// future (request cancellation, timeouts) therefore never abandons a
    /// half-mirrored git write — the batch still runs to completion, either
    /// converging with git truth or failing and flagging the index degraded.
    ///
    /// Panic supervision is owned by the worker itself (#314): a
    /// `DegradeOnDrop` guard inside the blocking closure marks the index
    /// rebuild-required if `apply` unwinds, so a panic at an unknown point
    /// is recorded even when the returned future was already dropped and no
    /// caller survives to observe the `JoinError`. A `JoinError` that *is*
    /// observed (worker never ran, e.g. shutdown) also marks the index —
    /// whether the commit landed is unknowable from a dead worker.
    pub fn apply_async(
        self: &Arc<Self>,
        ops: Vec<LexicalOp>,
    ) -> impl std::future::Future<Output = Result<(), MemoryError>> {
        let index = Arc::clone(self);
        let worker_guard_index = Arc::clone(self);
        let handle = traced_spawn_blocking(move || {
            let guard = DegradeOnDrop::new(worker_guard_index, "lexical apply worker panicked");
            let result = index.apply(ops);
            guard.defuse();
            result
        });
        let observer = Arc::clone(self);
        async move {
            match handle.await {
                Ok(result) => result,
                Err(e) => {
                    observer
                        .mark_rebuild_required("lexical apply worker did not run to completion");
                    Err(MemoryError::Join(e.to_string()))
                }
            }
        }
    }

    /// Insert or replace the document for `qualified_name`.
    ///
    /// Blocking convenience wrapper over [`Self::apply`] — one commit per
    /// call. Async callers batch through [`Self::apply_async`].
    pub fn upsert(
        &self,
        qualified_name: &str,
        name: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        self.apply(vec![LexicalOp::Upsert(LexicalDoc {
            qualified_name: qualified_name.to_string(),
            name: name.to_string(),
            content: content.to_string(),
        })])
    }

    /// Remove the document for `qualified_name`. No-op if absent.
    ///
    /// Blocking convenience wrapper over [`Self::apply`] — one commit per
    /// call. Async callers batch through [`Self::apply_async`].
    pub fn remove(&self, qualified_name: &str) -> Result<(), MemoryError> {
        self.apply(vec![LexicalOp::Remove(qualified_name.to_string())])
    }

    /// Replace the entire index contents with `docs` in a single commit.
    ///
    /// Convenience wrapper over [`Self::rebuild_from`] for callers whose
    /// `docs` are already in hand. When the document set comes from an async
    /// repo listing, capture the token with [`Self::begin_rebuild`] *before*
    /// listing and use [`Self::rebuild_from`] instead — otherwise divergence
    /// events between the listing and this call would be silently absorbed.
    pub fn rebuild<I>(&self, docs: I) -> Result<usize, MemoryError>
    where
        I: IntoIterator<Item = LexicalDoc>,
    {
        self.rebuild_from(self.begin_rebuild(), docs)
    }

    /// Replace the entire index contents with `docs` — the repo listing made
    /// after `token` was captured — in a single commit.
    ///
    /// Repair contract: on success the index is marked consistent with git
    /// truth *as of the token*. Divergence events after the capture keep the
    /// index degraded, and mirrors committed concurrently with the rebuild
    /// (which the listing may predate) re-flag it, so repeated repair
    /// deterministically converges. On failure the partial rebuild is rolled
    /// back and the index stays degraded.
    pub fn rebuild_from<I>(&self, token: RebuildToken, docs: I) -> Result<usize, MemoryError>
    where
        I: IntoIterator<Item = LexicalDoc>,
    {
        let span = tracing::debug_span!("lexical.rebuild", count = tracing::field::Empty);
        let _guard = span.enter();

        let inner = self.inner()?;
        let mut writer = match inner.lock_writer() {
            Ok(writer) => writer,
            Err(e) => {
                self.mark_rebuild_required("writer lock poisoned");
                return Err(e);
            }
        };

        let mut count = 0usize;
        let queued = (|| {
            writer.delete_all_documents().map_err(lexical_error)?;
            for item in docs {
                #[cfg(test)]
                self.check_failpoint(FailPoint::BeforeOp(count))?;
                let mut doc = TantivyDocument::default();
                doc.add_text(inner.field_qualified_name, &item.qualified_name);
                doc.add_text(inner.field_name, &item.name);
                doc.add_text(inner.field_content, &item.content);
                writer.add_document(doc).map_err(lexical_error)?;
                count += 1;
            }
            #[cfg(test)]
            self.check_failpoint(FailPoint::Commit)?;
            writer.commit().map_err(lexical_error)?;
            Ok(())
        })();

        if let Err(e) = queued {
            // Discard everything queued (including `delete_all_documents`)
            // so a later successful batch can never wipe the index.
            if let Err(rb) = writer.rollback() {
                warn!(error = %rb, "lexical writer rollback failed after rebuild error");
            }
            drop(writer);
            self.mark_rebuild_required("rebuild failed");
            return Err(e);
        }

        // Read the mirror counter while still holding the writer lock: any
        // apply that committed before this point is either reflected in the
        // listing or detected here; any apply after this point sees the
        // rebuilt contents.
        let raced_mirror = self.applied_batches.load(Ordering::Acquire) != token.applied;
        // Mark consistency as of the token's capture point. Divergence
        // events since then keep `dirty_epoch` ahead — still degraded.
        self.clean_epoch.fetch_max(token.dirty, Ordering::AcqRel);
        drop(writer);

        if raced_mirror {
            // A mirror committed between the token capture (pre-listing) and
            // the rebuild commit; the listing may not include it. Require
            // another rebuild rather than silently dropping the mirror.
            self.mark_rebuild_required("mirror raced the rebuild's repo listing");
        }

        #[cfg(test)]
        if let Err(e) = self.check_failpoint(FailPoint::Reload) {
            self.mark_rebuild_required("reader reload failed");
            return Err(e);
        }
        inner.reader.reload().map_err(|e| {
            self.mark_rebuild_required("reader reload failed");
            lexical_error(e)
        })?;
        tracing::Span::current().record("count", count);
        debug!(count, "lexical index rebuilt");
        Ok(count)
    }

    /// Run a BM25 keyword query, returning up to `limit`
    /// `(qualified_name, score)` pairs.
    ///
    /// Ranking is two-pass: documents matching the query as an *exact
    /// phrase* rank strictly above term-only matches. Plain BM25 term
    /// scoring lets a short document containing one query term outrank a
    /// long document containing the exact phrase (length normalisation
    /// dilutes the long document's term scores) — the failure mode behind
    /// #55. Within each pass, results are ordered by descending BM25 score
    /// with ties broken by ascending name, so the ranking is fully
    /// deterministic. Returned scores are only comparable within a pass;
    /// downstream fusion consumes ranks, not scores.
    ///
    /// Scope filtering matches recall semantics exactly: candidate scopes
    /// are parsed from the canonical key and checked with
    /// [`ScopeFilter::matches`].
    ///
    /// While the index is degraded ([`Self::is_degraded`]) — including for
    /// the whole duration of a repair rebuild — this errors instead of
    /// serving potentially stale results; hybrid recall treats the error as
    /// "semantic-only for this query".
    pub fn search(
        &self,
        filter: &ScopeFilter,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        let span = tracing::debug_span!("lexical.search", ?filter, limit);
        let _guard = span.enter();

        let inner = self.inner()?;
        if self.is_degraded() {
            return Err(MemoryError::Index(
                "lexical index degraded — rebuild from git truth pending".to_string(),
            ));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let searcher = inner.reader.searcher();
        if searcher.num_docs() == 0 {
            return Ok(Vec::new());
        }

        let mut results: Vec<(String, f32)> = Vec::new();

        // Pass 1: exact-phrase matches (only meaningful for multi-term queries).
        if let Some(phrase_query) = inner.phrase_query(query) {
            let phrase_hits = inner.execute(&searcher, &phrase_query, filter, limit)?;
            results.extend(phrase_hits);
        }

        // Pass 2: regular term matching, skipping documents pass 1 found.
        let parser =
            QueryParser::for_index(&inner.index, vec![inner.field_name, inner.field_content]);
        // Lenient parsing: user queries are natural language, not query
        // syntax — unbalanced quotes/parens degrade instead of erroring.
        let (parsed, parse_errors) = parser.parse_query_lenient(query);
        if !parse_errors.is_empty() {
            debug!(errors = ?parse_errors, "lexical query parsed leniently");
        }
        let term_hits = inner.execute(&searcher, &parsed, filter, limit)?;
        for hit in term_hits {
            if !results.iter().any(|(name, _)| name == &hit.0) {
                results.push(hit);
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    /// A `limit` at which [`Self::search`] is guaranteed to return every
    /// in-scope match: the number of indexed documents. It is conservative,
    /// not query-specific; a smaller limit may already suffice. A pass scans
    /// at most `max(MAX_CANDIDATES, limit)` matches, drops out-of-scope ones,
    /// and cuts the rest to `limit`, so below this bound a pass can hide
    /// in-scope matches even when it returns fewer than `limit`.
    pub(crate) fn search_bound(&self) -> usize {
        self.inner().map_or(0, |inner| {
            usize::try_from(inner.reader.searcher().num_docs()).unwrap_or(usize::MAX)
        })
    }
}

impl Inner {
    /// Build a phrase query matching the whole `query` as an exact phrase
    /// in either the name or content field. Returns `None` when the query
    /// tokenises to fewer than two terms (a phrase needs at least two).
    fn phrase_query(&self, query: &str) -> Option<Box<dyn Query>> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in [self.field_name, self.field_content] {
            let Ok(mut analyzer) = self.index.tokenizer_for_field(field) else {
                // Unreachable for TEXT fields with the default tokenizer;
                // skip this field's clause rather than the whole phrase pass.
                continue;
            };
            let mut terms: Vec<Term> = Vec::new();
            let mut stream = analyzer.token_stream(query);
            while let Some(token) = stream.next() {
                terms.push(Term::from_field_text(field, &token.text));
            }
            if terms.len() >= 2 {
                clauses.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
            }
        }
        if clauses.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::new(clauses)))
        }
    }

    /// Execute `query`, post-filter by scope, and return up to `limit`
    /// `(qualified_name, score)` pairs ordered by descending score with
    /// ties broken by ascending name.
    fn execute(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, MemoryError> {
        // Scope filtering happens after search, so collect enough candidates
        // to look past out-of-scope matches (bounded by MAX_CANDIDATES).
        let fetch = usize::try_from(searcher.num_docs().min(MAX_CANDIDATES))
            .unwrap_or(usize::MAX)
            .max(limit);
        let top_docs = searcher
            .search(query, &TopDocs::with_limit(fetch).order_by_score())
            .map_err(lexical_error)?;

        let mut results: Vec<(String, f32)> = Vec::new();
        for (score, address) in top_docs {
            let doc: TantivyDocument = searcher.doc(address).map_err(lexical_error)?;
            let Some(qualified_name) = doc
                .get_first(self.field_qualified_name)
                .and_then(|v| v.as_str())
            else {
                warn!("lexical index document is missing its qualified_name; skipping");
                continue;
            };
            let mref = match parse_qualified_name(qualified_name) {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        qualified_name = %qualified_name,
                        error = %e,
                        "could not parse qualified name from lexical index; skipping"
                    );
                    continue;
                }
            };
            if filter.matches(&mref.scope) {
                results.push((qualified_name.to_string(), score));
            }
        }

        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results.truncate(limit);
        Ok(results)
    }
}

fn lexical_error(e: tantivy::TantivyError) -> MemoryError {
    MemoryError::Index(format!("lexical index: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryName, MemoryRef, Scope, ScopePath};

    fn key(scope: &Scope, name: &str) -> String {
        MemoryRef::new(scope.clone(), MemoryName::new(name.to_string()).unwrap()).qualified_path()
    }

    fn scoped(path: &str) -> Scope {
        Scope::Path(ScopePath::new(path).unwrap())
    }

    fn subtree(path: &str) -> ScopeFilter {
        ScopeFilter::Subtree(ScopePath::new(path).unwrap())
    }

    fn index_with(docs: &[(&Scope, &str, &str)]) -> LexicalIndex {
        let index = LexicalIndex::new();
        for (scope, name, content) in docs {
            index
                .upsert(&key(scope, name), name, content)
                .expect("upsert");
        }
        index
    }

    #[test]
    fn keyword_match_is_found_and_scored() {
        let index = index_with(&[
            (
                &Scope::Root,
                "greeting",
                "happy birthday is a morning ritual",
            ),
            (&Scope::Root, "other", "kubernetes deployment configuration"),
        ]);

        let results = index
            .search(&ScopeFilter::All, "happy birthday", 10)
            .expect("search");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, key(&Scope::Root, "greeting"));
        assert!(results[0].1 > 0.0, "BM25 score should be positive");
    }

    #[test]
    fn memory_name_is_searchable() {
        let index = index_with(&[(&Scope::Root, "birthday-protocol", "unrelated body text")]);

        let results = index
            .search(&ScopeFilter::All, "birthday", 10)
            .expect("search");

        assert_eq!(results.len(), 1);
    }

    #[test]
    fn upsert_replaces_previous_document() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "note", "contains magicword here")]);

        index
            .upsert(&key(&scope, "note"), "note", "completely different now")
            .expect("upsert");

        let old = index
            .search(&ScopeFilter::All, "magicword", 10)
            .expect("search");
        assert!(old.is_empty(), "old content should no longer match");

        let new = index
            .search(&ScopeFilter::All, "different", 10)
            .expect("search");
        assert_eq!(new.len(), 1, "new content should match exactly once");
    }

    #[test]
    fn remove_makes_document_unreachable() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "doomed", "contains magicword here")]);

        index.remove(&key(&scope, "doomed")).expect("remove");

        let results = index
            .search(&ScopeFilter::All, "magicword", 10)
            .expect("search");
        assert!(results.is_empty());
    }

    #[test]
    fn scope_filter_root_only_excludes_scoped_memories() {
        let proj = scoped("proj");
        let index = index_with(&[
            (&Scope::Root, "root-note", "magicword in global"),
            (&proj, "proj-note", "magicword in project"),
        ]);

        let results = index
            .search(&ScopeFilter::RootOnly, "magicword", 10)
            .expect("search");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, key(&Scope::Root, "root-note"));
    }

    #[test]
    fn scope_filter_subtree_includes_root_and_descendants_only() {
        let team = scoped("org/team");
        let nested = scoped("org/team/project");
        let other = scoped("org/other");
        let index = index_with(&[
            (&Scope::Root, "root-note", "magicword in global"),
            (&team, "team-note", "magicword in team"),
            (&nested, "nested-note", "magicword in nested"),
            (&other, "other-note", "magicword in other"),
        ]);

        let results = index
            .search(&subtree("org/team"), "magicword", 10)
            .expect("search");

        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(results.len(), 3);
        assert!(names.contains(&key(&Scope::Root, "root-note").as_str()));
        assert!(names.contains(&key(&team, "team-note").as_str()));
        assert!(names.contains(&key(&nested, "nested-note").as_str()));
    }

    #[test]
    fn scope_filter_all_includes_everything() {
        let proj = scoped("proj");
        let index = index_with(&[
            (&Scope::Root, "root-note", "magicword in global"),
            (&proj, "proj-note", "magicword in project"),
        ]);

        let results = index
            .search(&ScopeFilter::All, "magicword", 10)
            .expect("search");

        assert_eq!(results.len(), 2);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let index = index_with(&[(&Scope::Root, "note", "Happy Birthday To You")]);

        let results = index
            .search(&ScopeFilter::All, "happy birthday", 10)
            .expect("search");

        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_syntax_errors_degrade_instead_of_failing() {
        let index = index_with(&[(&Scope::Root, "note", "unbalanced quote content")]);

        // Unbalanced quotes and parens must not produce an error.
        let result = index.search(&ScopeFilter::All, "\"unbalanced (quote", 10);
        assert!(result.is_ok(), "lenient parsing should never error");
    }

    #[test]
    fn empty_index_and_zero_limit_return_empty() {
        let index = LexicalIndex::new();
        assert!(index
            .search(&ScopeFilter::All, "anything", 10)
            .expect("search")
            .is_empty());

        index
            .upsert(&key(&Scope::Root, "note"), "note", "content")
            .expect("upsert");
        assert!(index
            .search(&ScopeFilter::All, "content", 0)
            .expect("search")
            .is_empty());
    }

    #[test]
    fn rebuild_replaces_all_contents() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "stale", "oldword content")]);

        let count = index
            .rebuild(vec![
                LexicalDoc {
                    qualified_name: key(&scope, "fresh-a"),
                    name: "fresh-a".to_string(),
                    content: "newword alpha".to_string(),
                },
                LexicalDoc {
                    qualified_name: key(&scope, "fresh-b"),
                    name: "fresh-b".to_string(),
                    content: "newword beta".to_string(),
                },
            ])
            .expect("rebuild");
        assert_eq!(count, 2);

        assert!(index
            .search(&ScopeFilter::All, "oldword", 10)
            .expect("search")
            .is_empty());
        assert_eq!(
            index
                .search(&ScopeFilter::All, "newword", 10)
                .expect("search")
                .len(),
            2
        );
    }

    #[test]
    fn exact_phrase_in_long_document_outranks_short_term_only_matches() {
        let scope = Scope::Root;
        // Long document: the exact phrase is buried in unrelated prose, so
        // BM25 length normalisation dilutes its per-term scores.
        let mut long_content = String::new();
        for i in 0..80 {
            long_content.push_str(&format!(
                "Filler sentence number {i} about infrastructure, deployments, and rust. "
            ));
        }
        long_content.push_str("the magic phrase appears exactly once here. ");
        for i in 0..80 {
            long_content.push_str(&format!("More filler prose {i} about unrelated topics. "));
        }

        let index = index_with(&[
            (&scope, "long-doc", long_content.as_str()),
            (&scope, "decoy-a", "magic tricks for beginners"),
            (&scope, "decoy-b", "phrase books for travellers"),
            (
                &scope,
                "decoy-c",
                "magic and phrase collections, magic themed",
            ),
        ]);

        let results = index
            .search(&ScopeFilter::All, "magic phrase", 10)
            .expect("search");

        assert_eq!(
            results[0].0,
            key(&scope, "long-doc"),
            "exact-phrase match must rank above term-only matches: {results:?}"
        );
    }

    #[test]
    fn apply_batches_mixed_ops_in_order_with_one_commit() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "doomed", "contains magicword here")]);

        // Remove-then-upsert of the same key inside one batch must yield a
        // fresh document, plus an unrelated upsert — all in a single apply.
        index
            .apply(vec![
                LexicalOp::Remove(key(&scope, "doomed")),
                LexicalOp::Upsert(LexicalDoc {
                    qualified_name: key(&scope, "doomed"),
                    name: "doomed".to_string(),
                    content: "reborn freshword".to_string(),
                }),
                LexicalOp::Upsert(LexicalDoc {
                    qualified_name: key(&scope, "extra"),
                    name: "extra".to_string(),
                    content: "another freshword".to_string(),
                }),
            ])
            .expect("apply");

        assert!(
            index
                .search(&ScopeFilter::All, "magicword", 10)
                .expect("search")
                .is_empty(),
            "old content must be gone after the batch"
        );
        assert_eq!(
            index
                .search(&ScopeFilter::All, "freshword", 10)
                .expect("search")
                .len(),
            2,
            "both batch upserts must be visible"
        );
    }

    #[test]
    fn apply_empty_batch_is_a_no_op() {
        let index = LexicalIndex::new();
        index.apply(Vec::new()).expect("empty batch is fine");
    }

    #[tokio::test]
    async fn apply_async_mutations_are_visible_after_await() {
        let scope = Scope::Root;
        let index = std::sync::Arc::new(LexicalIndex::new());

        index
            .apply_async(vec![LexicalOp::Upsert(LexicalDoc {
                qualified_name: key(&scope, "note"),
                name: "note".to_string(),
                content: "asyncword content".to_string(),
            })])
            .await
            .expect("apply_async");

        assert_eq!(
            index
                .search(&ScopeFilter::All, "asyncword", 10)
                .expect("search")
                .len(),
            1
        );
    }

    #[test]
    fn disabled_instance_errors_on_every_operation() {
        let index = LexicalIndex::disabled();

        assert!(index.upsert("key", "name", "content").is_err());
        assert!(index.remove("key").is_err());
        assert!(index
            .apply(vec![LexicalOp::Remove("key".to_string())])
            .is_err());
        assert!(index.rebuild(Vec::new()).is_err());
        assert!(index.search(&ScopeFilter::All, "query", 10).is_err());
    }

    // -----------------------------------------------------------------------
    // Failure/repair contract (#310)
    //
    // Written first as a characterization instrument: run against the
    // pre-contract code, the failing tests below are the evidence of where
    // failed mutations strand state. They assert the contract; the
    // implementation must make them pass and keep them green.
    // -----------------------------------------------------------------------

    fn ldoc(scope: &Scope, name: &str, content: &str) -> LexicalDoc {
        LexicalDoc {
            qualified_name: key(scope, name),
            name: name.to_string(),
            content: content.to_string(),
        }
    }

    /// Names returned for `query`, or `None` if the search itself errored
    /// (degraded mode — the caller falls back to semantic-only).
    fn search_names(index: &LexicalIndex, query: &str) -> Option<Vec<String>> {
        index
            .search(&ScopeFilter::All, query, 10)
            .ok()
            .map(|hits| hits.into_iter().map(|(n, _)| n).collect())
    }

    /// Poll until the index is consistent (`!is_degraded`) and `query`
    /// returns exactly `expected`, or panic after a deadline. Used for
    /// convergence assertions where a batch may complete on the blocking
    /// pool after the caller stopped waiting.
    async fn assert_converges(index: &LexicalIndex, query: &str, expected: &[String]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(hits) = search_names(index, query) {
                if hits == expected {
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "index never converged for query {query:?} (expected {expected:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Contract clause 2: ops queued before a mid-batch failure must never
    /// be committed later by an unrelated successful batch.
    #[test]
    fn failed_batch_prefix_must_not_ghost_commit_with_next_batch() {
        let scope = Scope::Root;
        let index = LexicalIndex::new();

        // Batch [ghost, doomed] fails before op 1 — "ghost" is already
        // queued on the Tantivy writer when the batch errors.
        index.fail_next(FailPoint::BeforeOp(1));
        assert!(index
            .apply(vec![
                LexicalOp::Upsert(ldoc(&scope, "ghost", "ghostword content")),
                LexicalOp::Upsert(ldoc(&scope, "doomed", "doomedword content")),
            ])
            .is_err());
        assert!(index.is_degraded(), "failed batch must flag the index");

        // Mirrors keep flowing while degraded…
        index
            .apply(vec![LexicalOp::Upsert(ldoc(&scope, "later", "laterword"))])
            .expect("later batch");

        // …and after repair from git truth the failed batch's prefix must
        // not have ghost-committed.
        index
            .rebuild(vec![ldoc(&scope, "later", "laterword")])
            .expect("repair rebuild");
        assert!(!index.is_degraded());
        let ghosts = search_names(&index, "ghostword").expect("post-repair search");
        assert!(
            ghosts.is_empty(),
            "prefix op of a failed batch ghost-committed: {ghosts:?}"
        );
        let later = search_names(&index, "laterword").expect("post-repair search");
        assert_eq!(later, vec![key(&scope, "later")]);
    }

    /// Contract clause 2: a batch whose commit fails must not be committed
    /// later by an unrelated successful batch.
    #[test]
    fn failed_commit_batch_must_not_apply_later() {
        let scope = Scope::Root;
        let index = LexicalIndex::new();

        index.fail_next(FailPoint::Commit);
        assert!(index
            .apply(vec![LexicalOp::Upsert(ldoc(
                &scope,
                "queued",
                "queuedword"
            ))])
            .is_err());
        assert!(index.is_degraded(), "failed commit must flag the index");

        index
            .apply(vec![LexicalOp::Upsert(ldoc(&scope, "later", "laterword"))])
            .expect("later batch");
        index
            .rebuild(vec![ldoc(&scope, "later", "laterword")])
            .expect("repair rebuild");
        let hits = search_names(&index, "queuedword").expect("post-repair search");
        assert!(
            hits.is_empty(),
            "batch with failed commit was committed later: {hits:?}"
        );
    }

    /// Contract clause 2 (worst case): a failed rebuild strands
    /// `delete_all_documents` on the writer; the next successful commit must
    /// not wipe the whole index.
    #[test]
    fn failed_rebuild_must_not_wipe_index_via_next_commit() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "keeper", "keeperword content")]);

        index.fail_next(FailPoint::BeforeOp(0));
        assert!(index
            .rebuild(vec![ldoc(&scope, "fresh", "freshword content")])
            .is_err());
        assert!(index.is_degraded(), "failed rebuild must flag the index");

        index
            .apply(vec![LexicalOp::Upsert(ldoc(&scope, "later", "laterword"))])
            .expect("later batch");

        // Repair from git truth: both memories exist in the repo.
        index
            .rebuild(vec![
                ldoc(&scope, "keeper", "keeperword content"),
                ldoc(&scope, "later", "laterword"),
            ])
            .expect("repair rebuild");
        assert!(!index.is_degraded());
        let keeper = search_names(&index, "keeperword").expect("post-repair search");
        assert_eq!(
            keeper,
            vec![key(&scope, "keeper")],
            "failed rebuild's stranded delete_all wiped the index"
        );
    }

    /// Contract clauses 2+3: after a failed mutation the index must not
    /// silently keep serving results that no longer reflect git truth —
    /// search degrades (errors) until repaired, and hybrid recall falls back
    /// to semantic-only.
    #[test]
    fn failed_mutation_must_not_leave_silent_stale_results() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "note", "oldword content")]);

        // Git truth moves to v2 ("newword"); the mirroring batch fails.
        index.fail_next(FailPoint::Commit);
        assert!(index
            .apply(vec![LexicalOp::Upsert(ldoc(
                &scope,
                "note",
                "newword content"
            ))])
            .is_err());

        // The index now diverges from git truth: it must refuse to serve
        // the stale v1 hit as a healthy result.
        assert!(index.is_degraded());
        assert!(
            index.search(&ScopeFilter::All, "oldword", 10).is_err(),
            "degraded index must error instead of serving stale results"
        );
    }

    /// Contract clause 4: repair after a failure is a rebuild from git truth
    /// and deterministically converges.
    #[test]
    fn rebuild_after_failure_restores_consistency() {
        let scope = Scope::Root;
        let index = index_with(&[(&scope, "note", "oldword content")]);

        index.fail_next(FailPoint::Commit);
        assert!(index
            .apply(vec![LexicalOp::Upsert(ldoc(
                &scope,
                "note",
                "newword content"
            ))])
            .is_err());

        // Repair: rebuild from git truth (v2).
        index
            .rebuild(vec![ldoc(&scope, "note", "newword content")])
            .expect("repair rebuild");
        assert!(!index.is_degraded());

        let hits = search_names(&index, "newword").expect("post-repair search must work");
        assert_eq!(hits, vec![key(&scope, "note")]);
        let stale = search_names(&index, "oldword").expect("post-repair search must work");
        assert!(stale.is_empty(), "stale content survived repair: {stale:?}");
    }

    /// Reload failure after a successful commit: the data IS committed but
    /// the caller was told the mutation failed. The contract resolves the
    /// report/truth mismatch by flagging degraded until repair.
    #[test]
    fn reload_failure_flags_degraded_and_repair_converges() {
        let scope = Scope::Root;
        let index = LexicalIndex::new();

        index.fail_next(FailPoint::Reload);
        assert!(index
            .apply(vec![LexicalOp::Upsert(ldoc(&scope, "note", "reloadword"))])
            .is_err());
        assert!(index.is_degraded(), "reload failure must flag the index");
        assert!(index.search(&ScopeFilter::All, "reloadword", 10).is_err());

        index
            .rebuild(vec![ldoc(&scope, "note", "reloadword")])
            .expect("repair rebuild");
        assert!(!index.is_degraded());
        let hits = search_names(&index, "reloadword").expect("post-repair search");
        assert_eq!(hits, vec![key(&scope, "note")]);
    }

    /// Contract clause 3: every failure stage flags the index degraded.
    #[test]
    fn every_failure_stage_marks_degraded() {
        let scope = Scope::Root;
        for point in [FailPoint::BeforeOp(0), FailPoint::Commit, FailPoint::Reload] {
            let index = LexicalIndex::new();
            index.fail_next(point);
            assert!(
                index
                    .apply(vec![LexicalOp::Upsert(ldoc(&scope, "note", "word"))])
                    .is_err(),
                "{point:?} must error"
            );
            assert!(index.is_degraded(), "{point:?} must flag degraded");
        }
    }

    /// A divergence event *after* the rebuild token was captured must keep
    /// the index degraded even though the rebuild itself succeeds — the
    /// rebuild's document set cannot reflect it.
    #[test]
    fn divergence_after_token_capture_keeps_degraded() {
        let scope = Scope::Root;
        let index = LexicalIndex::new();

        let token = index.begin_rebuild();
        // Simulates a mirror failing between the repo listing and the
        // rebuild commit.
        index.mark_rebuild_required("test: failure after token capture");

        index
            .rebuild_from(token, vec![ldoc(&scope, "note", "someword")])
            .expect("rebuild itself succeeds");
        assert!(
            index.is_degraded(),
            "a post-capture divergence event must survive the rebuild"
        );

        // A follow-up rebuild with a fresh token converges.
        index
            .rebuild(vec![ldoc(&scope, "note", "someword")])
            .expect("follow-up rebuild");
        assert!(!index.is_degraded());
    }

    /// A mirror committed after the token capture (i.e. racing the repo
    /// listing) must re-flag the index: the rebuild's document set may not
    /// include it. A follow-up quiescent rebuild converges.
    #[test]
    fn mirror_racing_rebuild_listing_keeps_degraded() {
        let scope = Scope::Root;
        let index = LexicalIndex::new();

        let token = index.begin_rebuild();
        // Mirror of a git write that the (already taken) listing missed.
        index
            .apply(vec![LexicalOp::Upsert(ldoc(&scope, "raced", "racedword"))])
            .expect("raced mirror");

        index
            .rebuild_from(token, vec![ldoc(&scope, "old", "oldword")])
            .expect("rebuild succeeds");
        assert!(
            index.is_degraded(),
            "a mirror racing the listing must keep the index flagged"
        );

        // Follow-up rebuild from current git truth converges.
        index
            .rebuild(vec![
                ldoc(&scope, "old", "oldword"),
                ldoc(&scope, "raced", "racedword"),
            ])
            .expect("follow-up rebuild");
        assert!(!index.is_degraded());
        let hits = search_names(&index, "racedword").expect("post-repair search");
        assert_eq!(hits, vec![key(&scope, "raced")]);
    }

    /// The repair slot is single-flight.
    #[test]
    fn repair_slot_is_single_flight() {
        let index = LexicalIndex::new();
        assert!(index.try_claim_repair());
        assert!(index.is_repairing());
        assert!(!index.try_claim_repair(), "second claim must fail");
        index.finish_repair();
        assert!(!index.is_repairing());
        assert!(index.try_claim_repair(), "slot must be reusable");
    }

    /// Contract clause 5: dropping the `apply_async` future — polled or not —
    /// never strands the batch. Dispatch is eager, so the batch runs to
    /// completion on the blocking pool and converges with git truth.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_apply_async_after_dispatch_converges() {
        let scope = Scope::Root;
        let index = Arc::new(LexicalIndex::new());

        let fut = index.apply_async(vec![LexicalOp::Upsert(ldoc(
            &scope,
            "note",
            "cancelword content",
        ))]);
        // Poll once, then drop the future (request cancellation).
        let cancelled = tokio::time::timeout(std::time::Duration::from_nanos(1), fut)
            .await
            .is_err();

        // Whether or not the timeout won the race, the batch must converge.
        tracing::debug!(cancelled, "apply_async cancellation raced");
        assert_converges(&index, "cancelword", &[key(&scope, "note")]).await;
    }

    /// Contract clause 5 (the pre-#310 strand case): a future dropped before
    /// its first poll. Eager dispatch means the batch has already been handed
    /// to the blocking pool at call time, so the mutation still converges
    /// instead of being silently lost after its git write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_async_dropped_unpolled_still_converges() {
        let scope = Scope::Root;
        let index = Arc::new(LexicalIndex::new());

        let fut = index.apply_async(vec![LexicalOp::Upsert(ldoc(
            &scope,
            "note",
            "unpolledword content",
        ))]);
        drop(fut);

        assert_converges(&index, "unpolledword", &[key(&scope, "note")]).await;
    }

    /// ADR-0039 status surface: every lifecycle state is explicit and
    /// unambiguous — no error-string inference required.
    #[test]
    fn status_reports_explicit_lifecycle_states() {
        let scope = Scope::Root;

        let disabled = LexicalIndex::disabled();
        assert!(!disabled.is_available());
        assert_eq!(disabled.status(), LexicalStatus::Failed);

        let index = LexicalIndex::new();
        assert!(index.is_available());
        assert_eq!(index.status(), LexicalStatus::Available);

        index.mark_rebuild_required("test: forced divergence");
        assert_eq!(index.status(), LexicalStatus::Degraded);

        assert!(index.try_claim_repair());
        assert_eq!(index.status(), LexicalStatus::Repairing);
        index.finish_repair();
        assert_eq!(index.status(), LexicalStatus::Degraded);

        index
            .rebuild(vec![ldoc(&scope, "note", "statusword")])
            .expect("rebuild");
        assert_eq!(index.status(), LexicalStatus::Available);
    }

    /// A disabled index also accumulates divergence events (its failed ops
    /// call `mark_rebuild_required`), but construction failure dominates:
    /// the status must stay `Failed`, never masquerade as repairable
    /// `Degraded`.
    #[test]
    fn disabled_index_status_is_failed_even_after_divergence_events() {
        let disabled = LexicalIndex::disabled();
        assert!(disabled
            .apply(vec![LexicalOp::Remove("key".to_string())])
            .is_err());
        assert!(disabled.is_degraded());
        assert_eq!(disabled.status(), LexicalStatus::Failed);
    }

    #[test]
    fn results_are_deterministically_ordered() {
        let scope = Scope::Root;
        // Two documents with identical content produce identical BM25
        // scores — ordering must fall back to the qualified name.
        let index = index_with(&[
            (&scope, "zeta", "identical magicword content"),
            (&scope, "alpha", "identical magicword content"),
        ]);

        let results = index
            .search(&ScopeFilter::All, "magicword", 10)
            .expect("search");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, key(&scope, "alpha"));
        assert_eq!(results[1].0, key(&scope, "zeta"));
    }
}
