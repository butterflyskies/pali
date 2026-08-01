//! Tracing scaffold integration tests.
//!
//! These tests verify:
//! - TC-01/TC-02: Span names follow the `module.operation` pattern.
//! - TC-04:       Span field names are from the canonical allowlist.
//! - TC-06–TC-09: Subsystem spans carry the correct fields.
//! - TC-16–TC-18: No sensitive data (tokens, content text, raw URLs) in spans.
//! - TC-20:       Default filter passes handler info spans, suppresses debug spans.
//! - TC-21:       Push failure does not leak token values in traces.
//!
//! The in-memory subscriber collects span/event metadata so we can assert on
//! it without touching real networking or the file system (where possible).
//!
//! **Note:** `with_capturing` uses `tracing::subscriber::with_default`, which is
//! thread-local. Production code uses `traced_spawn_blocking` to propagate the
//! dispatch into blocking threads, so span records and events are visible to tests.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use tracing::{subscriber::with_default, Subscriber};
use tracing_subscriber::{
    layer::{Context, Layer, SubscriberExt},
    Registry,
};

// ---------------------------------------------------------------------------
// In-memory span / event record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SpanRecord {
    /// Span name.
    name: String,
    /// tracing::Level (used by TC-20 to filter debug spans)
    #[allow(dead_code)]
    level: tracing::Level,
    /// Field key-value pairs present on the span (key, value as Display string).
    fields: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct EventRecord {
    /// Message / event target.
    message: String,
    /// Key=value fields as strings (values are formatted via Display).
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct RecordStore {
    spans: Vec<SpanRecord>,
    events: Vec<EventRecord>,
}

/// A `tracing_subscriber::Layer` that records spans and events.
struct CapturingLayer {
    store: Arc<Mutex<RecordStore>>,
}

/// Visitor that collects field (name, value) pairs from spans.
struct KvVisitor(Vec<(String, String)>);

impl tracing::field::Visit for KvVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, val: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_string(), format!("{val:?}")));
    }
    fn record_str(&mut self, field: &tracing::field::Field, val: &str) {
        self.0.push((field.name().to_string(), val.to_string()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, val: i64) {
        self.0.push((field.name().to_string(), val.to_string()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, val: u64) {
        self.0.push((field.name().to_string(), val.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, val: bool) {
        self.0.push((field.name().to_string(), val.to_string()));
    }
}

impl<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S>
    for CapturingLayer
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = KvVisitor(Vec::new());
        attrs.record(&mut visitor);
        let mut kv = visitor.0;

        // Also capture field names that are declared Empty (no value yet).
        for field in attrs.fields() {
            let n = field.name().to_string();
            if !kv.iter().any(|(k, _)| k == &n) {
                kv.push((n, String::new()));
            }
        }

        let record = SpanRecord {
            name: attrs.metadata().name().to_string(),
            level: *attrs.metadata().level(),
            fields: kv.clone(),
        };
        self.store.lock().unwrap().spans.push(record);

        // Store the kv pairs as span extensions so on_record can update them.
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(kv);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = KvVisitor(Vec::new());
        values.record(&mut visitor);
        let new_kv = visitor.0;

        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(kv) = ext.get_mut::<Vec<(String, String)>>() {
                for (new_key, new_val) in &new_kv {
                    if let Some(existing) = kv.iter_mut().find(|(k, _)| k == new_key) {
                        existing.1 = new_val.clone();
                    } else {
                        kv.push((new_key.clone(), new_val.clone()));
                    }
                }
                // Update the corresponding SpanRecord in the store.
                let span_name = span.name().to_string();
                drop(ext);
                let mut store = self.store.lock().unwrap();
                // Find the last span with this name and update its fields.
                // Note: name-based lookup means multi-call tests for the same
                // operation will update the wrong SpanRecord. Sufficient for
                // current tests (one call per operation per with_capturing scope).
                if let Some(rec) = store.spans.iter_mut().rev().find(|s| s.name == span_name) {
                    for (new_key, new_val) in &new_kv {
                        if let Some(existing) = rec.fields.iter_mut().find(|(k, _)| k == new_key) {
                            existing.1 = new_val.clone();
                        } else {
                            rec.fields.push((new_key.clone(), new_val.clone()));
                        }
                    }
                }
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut kv: Vec<(String, String)> = Vec::new();
        let mut message = String::new();

        struct EventVisitor<'a> {
            kv: &'a mut Vec<(String, String)>,
            message: &'a mut String,
        }
        impl<'a> tracing::field::Visit for EventVisitor<'a> {
            fn record_debug(&mut self, field: &tracing::field::Field, val: &dyn std::fmt::Debug) {
                let s = format!("{val:?}");
                if field.name() == "message" {
                    *self.message = s;
                } else {
                    self.kv.push((field.name().to_string(), s));
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, val: &str) {
                if field.name() == "message" {
                    *self.message = val.to_string();
                } else {
                    self.kv.push((field.name().to_string(), val.to_string()));
                }
            }
            fn record_i64(&mut self, field: &tracing::field::Field, val: i64) {
                self.kv.push((field.name().to_string(), val.to_string()));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, val: u64) {
                self.kv.push((field.name().to_string(), val.to_string()));
            }
            fn record_bool(&mut self, field: &tracing::field::Field, val: bool) {
                self.kv.push((field.name().to_string(), val.to_string()));
            }
        }

        event.record(&mut EventVisitor {
            kv: &mut kv,
            message: &mut message,
        });

        self.store.lock().unwrap().events.push(EventRecord {
            message,
            fields: kv,
        });
    }
}

/// Panic unless running under nextest (process-per-test isolation).
///
/// Tests that use `tracing::subscriber::with_default` + `traced_spawn_blocking`
/// rely on thread-local dispatch propagation that is unsound under `cargo test`
/// (shared process, concurrent tests). Nextest sets `NEXTEST=1` in every child.
fn require_nextest() {
    assert!(
        std::env::var("NEXTEST").is_ok(),
        "this test requires nextest (cargo nextest run)"
    );
}

/// Build a subscriber with the capturing layer installed and run `f` inside it.
fn with_capturing<F, R>(f: F) -> (R, Arc<Mutex<RecordStore>>)
where
    F: FnOnce() -> R,
{
    require_nextest();
    let store = Arc::new(Mutex::new(RecordStore::default()));
    let layer = CapturingLayer {
        store: Arc::clone(&store),
    };
    let subscriber = Registry::default().with(layer);
    let result = with_default(subscriber, f);
    (result, store)
}

// ---------------------------------------------------------------------------
// Canonical field allowlist (TC-04)
// ---------------------------------------------------------------------------

fn canonical_fields() -> HashSet<&'static str> {
    [
        "session_id",
        "name",
        "scope",
        "content_size",
        "batch_size",
        "chunk_size",
        "dimensions",
        "model",
        "count",
        "key_count",
        "branch",
        "file_count",
        "oid",
        "token_source",
        "duration_ms",
        // Move-related fields (handler.move, repo.move)
        "source_name",
        "dest_name",
        "new_name",
        "from_scope",
        "to_scope",
        // These are tracing internal / log fields we allow through:
        "message",
        "error",
        "pull_first",
        "limit",
        // tracing instrumentation fields
        "log.target",
        "log.module_path",
        "log.file",
        "log.line",
    ]
    .into_iter()
    .collect()
}

// ---------------------------------------------------------------------------
// TC-01/TC-02: Span names follow `module.operation` pattern
// ---------------------------------------------------------------------------

#[test]
fn index_add_span_has_correct_name() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc01-test".to_string());
    });
    let spans = store.lock().unwrap();
    let names: Vec<&str> = spans.spans.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"index.add"),
        "expected 'index.add' span, got: {names:?}"
    );
}

#[test]
fn index_remove_span_has_correct_name() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc01-remove".to_string());
        let _ = idx.remove(&Scope::Root, "global/tc01-remove");
    });
    let spans = store.lock().unwrap();
    let names: Vec<&str> = spans.spans.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"index.remove"),
        "expected 'index.remove' span, got: {names:?}"
    );
}

#[test]
fn index_search_span_has_correct_name() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::{Scope, ScopeFilter};

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc01-search".to_string());
        let _ = idx.search(&ScopeFilter::RootOnly, &v, 5);
    });
    let spans = store.lock().unwrap();
    let names: Vec<&str> = spans.spans.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"index.search"),
        "expected 'index.search' span, got: {names:?}"
    );
}

#[test]
fn index_save_load_spans_have_correct_names() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    let dir = tempfile::tempdir().expect("tempdir");
    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc01-save".to_string());
        let _ = idx.save(dir.path());
        let _ = UsearchStore::load(dir.path(), 4);
    });
    let spans = store.lock().unwrap();
    let names: Vec<&str> = spans.spans.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"index.save"),
        "expected 'index.save' span, got: {names:?}"
    );
    assert!(
        names.contains(&"index.load"),
        "expected 'index.load' span, got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// TC-04: Fields on index spans are in canonical allowlist
// ---------------------------------------------------------------------------

#[test]
fn index_spans_only_use_canonical_fields() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::{Scope, ScopeFilter};

    let dir = tempfile::tempdir().expect("tempdir");
    let allowed = canonical_fields();

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc04-test".to_string());
        let _ = idx.remove(&Scope::Root, "global/tc04-test");
        let _ = idx.search(&ScopeFilter::All, &v, 5);
        let _ = idx.save(dir.path());
        let _ = UsearchStore::load(dir.path(), 4);
    });
    let spans = store.lock().unwrap();
    // Only check index.* spans.
    let index_spans: Vec<&SpanRecord> = spans
        .spans
        .iter()
        .filter(|s| s.name.starts_with("index."))
        .collect();
    assert!(
        !index_spans.is_empty(),
        "expected at least one index.* span"
    );
    for span in &index_spans {
        for (field_name, _) in &span.fields {
            assert!(
                allowed.contains(field_name.as_str()),
                "span '{}' has field '{}' not in canonical allowlist",
                span.name,
                field_name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// TC-06: index.add carries scope, key_count, dimensions
// ---------------------------------------------------------------------------

#[test]
fn index_add_span_has_scope_and_dimensions_fields() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(8).expect("create index");
        let v = vec![0.0_f32; 8];
        let _ = idx.add(&Scope::Root, &v, "global/tc06-test".to_string());
    });
    let spans = store.lock().unwrap();
    let add_span = spans
        .spans
        .iter()
        .find(|s| s.name == "index.add")
        .expect("index.add span not found");

    assert!(
        add_span.fields.iter().any(|(k, _)| k == "scope"),
        "index.add missing 'scope' field"
    );
    assert!(
        add_span.fields.iter().any(|(k, _)| k == "dimensions"),
        "index.add missing 'dimensions' field"
    );
    assert!(
        add_span.fields.iter().any(|(k, _)| k == "key_count"),
        "index.add missing 'key_count' field"
    );
}

// ---------------------------------------------------------------------------
// TC-07: index.search carries scope, count, dimensions
// ---------------------------------------------------------------------------

#[test]
fn index_search_span_has_required_fields() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::{Scope, ScopeFilter};

    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc07".to_string());
        let _ = idx.search(&ScopeFilter::RootOnly, &v, 5);
    });
    let spans = store.lock().unwrap();
    let search_span = spans
        .spans
        .iter()
        .find(|s| s.name == "index.search")
        .expect("index.search span not found");

    assert!(
        search_span.fields.iter().any(|(k, _)| k == "scope"),
        "index.search missing 'scope' field"
    );
    assert!(
        search_span.fields.iter().any(|(k, _)| k == "dimensions"),
        "index.search missing 'dimensions' field"
    );
    assert!(
        search_span.fields.iter().any(|(k, _)| k == "count"),
        "index.search missing 'count' field"
    );
    assert!(
        search_span.fields.iter().any(|(k, _)| k == "key_count"),
        "index.search missing 'key_count' field"
    );
}

// ---------------------------------------------------------------------------
// TC-09: index.save carries key_count
// ---------------------------------------------------------------------------

#[test]
fn index_save_span_has_key_count_field() {
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    let dir = tempfile::tempdir().expect("tempdir");
    let (_, store) = with_capturing(|| {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc09-save".to_string());
        let _ = idx.save(dir.path());
    });
    let spans = store.lock().unwrap();
    let save_span = spans
        .spans
        .iter()
        .find(|s| s.name == "index.save")
        .expect("index.save span not found");

    assert!(
        save_span.fields.iter().any(|(k, _)| k == "key_count"),
        "index.save missing 'key_count' field"
    );
}

// ---------------------------------------------------------------------------
// TC-16: No token values in any span field
// ---------------------------------------------------------------------------

#[test]
fn auth_resolution_never_logs_token_value() {
    use pali::auth::AuthProvider;

    let token_value = "ghp_super_secret_test_token_12345";
    let (_, store) = with_capturing(|| {
        // Set env var with a "secret" token and attempt resolution.
        std::env::set_var("MEMORY_MCP_GITHUB_TOKEN", token_value);
        let provider = AuthProvider::new();
        let _ = provider.resolve_token();
        std::env::remove_var("MEMORY_MCP_GITHUB_TOKEN");
    });
    let store = store.lock().unwrap();

    // Check no span field VALUE contains the raw token value.
    for span in &store.spans {
        for (field_name, field_value) in &span.fields {
            assert!(
                !field_value.contains(token_value),
                "span '{}' field '{}' value contains raw token: {field_value}",
                span.name,
                field_name
            );
        }
    }

    // Check no event field value contains the raw token value.
    for event in &store.events {
        for (k, v) in &event.fields {
            assert!(
                !v.contains(token_value),
                "event field {k}={v:?} contains raw token"
            );
        }
        assert!(
            !event.message.contains(token_value),
            "event message contains raw token: {:?}",
            event.message
        );
    }
}

// ---------------------------------------------------------------------------
// TC-18: URLs are redacted (no userinfo in repo spans)
// ---------------------------------------------------------------------------

#[test]
fn repo_init_url_is_redacted_in_logs() {
    use pali::repo::MemoryRepo;

    // Use a URL containing a fake credential.
    // git2 file:// URLs don't contain userinfo, but let's verify our
    // redact_url logic: the logs from init_or_open go through redact_url.
    let dir = tempfile::tempdir().expect("tempdir");
    let fake_url = "https://x-access-token:ghp_faketoken@github.com/owner/repo.git";

    let (_, store) = with_capturing(|| {
        // This will fail (no real GitHub), but log the URL via redact_url.
        // We just need to verify the token doesn't appear in traces.
        let _result = MemoryRepo::init_or_open(dir.path(), Some(fake_url));
    });
    let store = store.lock().unwrap();

    for event in &store.events {
        assert!(
            !event.message.contains("ghp_faketoken"),
            "event message contains raw token in URL: {:?}",
            event.message
        );
        for (k, v) in &event.fields {
            assert!(
                !v.contains("ghp_faketoken"),
                "event field {k}={v:?} contains raw token in URL"
            );
        }
    }
    for span in &store.spans {
        for (field_name, field_value) in &span.fields {
            assert!(
                !field_value.contains("ghp_faketoken"),
                "span '{}' field '{}' value contains raw token in URL: {field_value}",
                span.name,
                field_name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// TC-21: Push failure does not leak token in tracing output
// ---------------------------------------------------------------------------

#[test]
fn push_failure_does_not_leak_token() {
    use pali::auth::AuthProvider;
    use pali::repo::MemoryRepo;
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let token_value = "ghp_leaked_token_value_12345";

    let (_, store) = with_capturing(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let repo = Arc::new(
                MemoryRepo::init_or_open(dir.path(), Some("https://github.com/fake/repo.git"))
                    .expect("init repo"),
            );
            let auth = AuthProvider::with_token(token_value);
            let result = repo.push(&auth, "main").await;
            assert!(result.is_err(), "push to fake remote should fail");
        });
    });
    let store = store.lock().unwrap();

    for span in &store.spans {
        for (k, v) in &span.fields {
            assert!(
                !v.contains(token_value),
                "span '{}' field '{k}' leaks token: {v}",
                span.name
            );
        }
    }
    for event in &store.events {
        for (k, v) in &event.fields {
            assert!(
                !v.contains(token_value),
                "event field '{k}' leaks token: {v}"
            );
        }
        assert!(
            !event.message.contains(token_value),
            "event message leaks token: {:?}",
            event.message
        );
    }
}

// ---------------------------------------------------------------------------
// TC-20: Default filter — index.* debug spans should be suppressible
// ---------------------------------------------------------------------------

#[test]
fn debug_spans_are_filtered_when_only_info_enabled() {
    require_nextest();
    use pali::index::{UsearchStore, VectorStore};
    use pali::types::Scope;

    // --- Baseline: verify index.* spans DO appear under DEBUG filter ---
    let baseline_store = Arc::new(Mutex::new(RecordStore::default()));
    {
        let layer = CapturingLayer {
            store: Arc::clone(&baseline_store),
        };
        let filter = tracing_subscriber::EnvFilter::new("debug");
        let subscriber = Registry::default().with(layer).with(filter);
        with_default(subscriber, || {
            let idx = UsearchStore::new(4).expect("create index");
            let v = vec![1.0_f32, 0.0, 0.0, 0.0];
            let _ = idx.add(&Scope::Root, &v, "global/tc20-baseline".to_string());
        });
    }
    {
        let spans = baseline_store.lock().unwrap();
        let index_spans: Vec<&SpanRecord> = spans
            .spans
            .iter()
            .filter(|s| s.name.starts_with("index."))
            .collect();
        assert!(
            !index_spans.is_empty(),
            "baseline: expected index.* spans to appear under DEBUG filter"
        );
    }

    // --- Main assertion: index.* debug spans suppressed under INFO filter ---
    let store = Arc::new(Mutex::new(RecordStore::default()));
    let layer = CapturingLayer {
        store: Arc::clone(&store),
    };
    let filter = tracing_subscriber::EnvFilter::new("info");
    let subscriber = Registry::default().with(layer).with(filter);

    with_default(subscriber, || {
        let idx = UsearchStore::new(4).expect("create index");
        let v = vec![1.0_f32, 0.0, 0.0, 0.0];
        let _ = idx.add(&Scope::Root, &v, "global/tc20-filter".to_string());
    });

    let spans = store.lock().unwrap();
    // Under INFO filter, index.add (debug level) should NOT be recorded.
    let debug_spans: Vec<&SpanRecord> = spans
        .spans
        .iter()
        .filter(|s| s.name.starts_with("index."))
        .collect();
    assert!(
        debug_spans.is_empty(),
        "debug-level index.* spans should be filtered under INFO: {debug_spans:?}"
    );
}

// ---------------------------------------------------------------------------
// TC-08: repo.* spans have correct field names
// ---------------------------------------------------------------------------

#[test]
fn repo_save_span_has_name_and_oid_fields() {
    use pali::repo::MemoryRepo;
    use pali::types::{Memory, MemoryMetadata, Scope};
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");

    let (_, store) = with_capturing(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let repo = Arc::new(MemoryRepo::init_or_open(dir.path(), None).expect("init repo"));
            let meta = MemoryMetadata {
                tags: vec![],
                scope: Scope::Root,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                source: None,
            };
            let mem = Memory::new("tc08-memory", "test content", meta).unwrap();
            repo.save_memory(&mem).await.expect("save");
        });
    });
    let spans = store.lock().unwrap();
    let save_span = spans
        .spans
        .iter()
        .find(|s| s.name == "repo.save")
        .expect("repo.save span not found");

    assert!(
        save_span.fields.iter().any(|(k, _)| k == "name"),
        "repo.save missing 'name' field; fields: {:?}",
        save_span.fields
    );
    assert!(
        save_span
            .fields
            .iter()
            .any(|(k, v)| k == "oid" && !v.is_empty()),
        "repo.save 'oid' field missing or empty; fields: {:?}",
        save_span.fields
    );
}

// ---------------------------------------------------------------------------
// TC-08b: repo.delete span has name and oid fields
// ---------------------------------------------------------------------------

#[test]
fn repo_delete_span_has_name_and_oid_fields() {
    use pali::repo::MemoryRepo;
    use pali::types::{Memory, MemoryMetadata, Scope};
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");

    let (_, store) = with_capturing(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let repo = Arc::new(MemoryRepo::init_or_open(dir.path(), None).expect("init repo"));
            let meta = MemoryMetadata {
                tags: vec![],
                scope: Scope::Root,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                source: None,
            };
            let mem = Memory::new("tc08b-memory", "test content", meta).unwrap();
            repo.save_memory(&mem).await.expect("save");
            repo.delete_memory("tc08b-memory", &Scope::Root)
                .await
                .expect("delete");
        });
    });
    let spans = store.lock().unwrap();
    let delete_span = spans
        .spans
        .iter()
        .find(|s| s.name == "repo.delete")
        .expect("repo.delete span not found");

    assert!(
        delete_span.fields.iter().any(|(k, _)| k == "name"),
        "repo.delete missing 'name' field; fields: {:?}",
        delete_span.fields
    );
    assert!(
        delete_span
            .fields
            .iter()
            .any(|(k, v)| k == "oid" && !v.is_empty()),
        "repo.delete 'oid' field missing or empty; fields: {:?}",
        delete_span.fields
    );
}

// ---------------------------------------------------------------------------
// TC-17: content text not in any span field
// ---------------------------------------------------------------------------

#[test]
fn repo_save_does_not_log_content_text() {
    use pali::repo::MemoryRepo;
    use pali::types::{Memory, MemoryMetadata, Scope};
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let secret_content = "top_secret_content_abc123";

    let (_, store) = with_capturing(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let repo = Arc::new(MemoryRepo::init_or_open(dir.path(), None).expect("init repo"));
            let meta = MemoryMetadata {
                tags: vec![],
                scope: Scope::Root,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                source: None,
            };
            let mem = Memory::new("tc17-memory", secret_content, meta).unwrap();
            repo.save_memory(&mem).await.expect("save");
        });
    });
    let store = store.lock().unwrap();

    // Verify no span field VALUE contains the secret content text.
    for span in &store.spans {
        for (field_name, field_value) in &span.fields {
            assert!(
                !field_value.contains(secret_content),
                "span '{}' field '{}' value contains content text: {field_value}",
                span.name,
                field_name
            );
        }
    }
    // Verify no event message or field value contains the secret content text.
    for event in &store.events {
        assert!(
            !event.message.contains(secret_content),
            "event message contains content text: {:?}",
            event.message
        );
        for (k, v) in &event.fields {
            assert!(
                !v.contains(secret_content),
                "event field {k}={v:?} contains content text"
            );
        }
    }
}
