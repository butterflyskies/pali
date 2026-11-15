//! Shared test helpers for in-process integration tests.
//!
//! Builds the full service stack (StreamableHttpService + axum routes) using
//! offline backends so tests can call via `tower::ServiceExt::oneshot` without
//! spawning a subprocess or binding to a real port. The default embedding
//! backend is deterministic and can populate the store, so a test that writes
//! a memory and reads it back exercises real data rather than an empty index.
//!
//! Each integration test is its own crate and uses only some of these
//! helpers, so the rest would warn as dead code there.
#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use mcp_session::BoundedSessionManagerBuilder;
use pali::{
    auth::AuthProvider,
    embedding::EmbeddingBackend,
    error::MemoryError,
    health::{healthz_handler, readyz_handler, version_handler, HealthRegistry},
    index::InMemoryStore,
    repo::MemoryRepo,
    server::MemoryServer,
    types::AppState,
};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Deterministic embedding backend
// ---------------------------------------------------------------------------

/// Dimensionality of every [`StubEmbeddingBackend`] vector.
pub const HASH_EMBEDDING_DIMS: usize = 256;

/// Words too common to carry topic signal in the hashed bag of words.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "are", "was", "but", "not", "you", "your", "from",
    "into", "then", "than", "each", "all", "any", "one", "two", "its", "has", "have", "who", "out",
    "off", "our", "can", "will", "would", "what", "when", "only", "also", "per",
];

/// The test embedding backend: a hashed bag of features, deterministic and
/// offline. It is the one embedder every in-process test shares, so the
/// access fixture's record and the isolation tests rank on the same vectors.
///
/// Lower-cased alphanumeric words of three or more characters (minus stop
/// words) count at weight 1, plus every character trigram of those words at
/// weight 1/4, hashed with FNV-1a into [`HASH_EMBEDDING_DIMS`] signed
/// buckets, then L2-normalised. Words keep related notes near each other, so
/// recall ranking reflects content; trigrams make texts that share no word
/// still differ in distance, because `InMemoryStore` orders equal distances
/// arbitrarily. A text with no usable tokens maps to a fixed unit vector, so
/// every output is normalised and non-zero.
///
/// Health is controlled via the [`HealthRegistry`], not this backend.
pub struct StubEmbeddingBackend;

impl StubEmbeddingBackend {
    /// Embed one text. Public so a test can compute distances itself.
    pub fn embed_text(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; HASH_EMBEDDING_DIMS];
        let lower = text.to_lowercase();
        for word in lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= 3 && !STOP_WORDS.contains(t))
        {
            add_feature(&mut vector, word, 1.0);
            let chars: Vec<char> = word.chars().collect();
            for window in chars.windows(3) {
                let trigram: String = window.iter().collect();
                add_feature(&mut vector, &format!("#{trigram}"), 0.25);
            }
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 {
            // A text with no usable tokens gets a fixed unit vector rather
            // than a zero vector, whose cosine similarity would be NaN.
            vector[0] = 1.0;
        } else {
            for v in &mut vector {
                *v /= norm;
            }
        }
        vector
    }
}

fn fnv1a(token: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let hash = fnv1a(feature);
    let bucket = usize::try_from(hash % HASH_EMBEDDING_DIMS as u64).expect("bucket fits in usize");
    let sign = if hash & (1 << 32) == 0 { 1.0 } else { -1.0 };
    vector[bucket] += sign * weight;
}

#[async_trait]
impl EmbeddingBackend for StubEmbeddingBackend {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(texts.iter().map(|text| Self::embed_text(text)).collect())
    }

    fn dimensions(&self) -> usize {
        HASH_EMBEDDING_DIMS
    }
}

// ---------------------------------------------------------------------------
// AppState builder
// ---------------------------------------------------------------------------

/// Build an [`AppState`] with the given embedding backend, an
/// [`InMemoryStore`] sized to that backend, and a fresh temp-dir repo.
///
/// Use this only to substitute a deliberately faulty backend; ordinary tests
/// should take [`build_stub_state`], whose backend can populate the store.
pub fn build_state_with_embedding(
    tmp: &tempfile::TempDir,
    health: HealthRegistry,
    embedding: Box<dyn EmbeddingBackend>,
) -> Arc<AppState> {
    let repo = MemoryRepo::init_or_open(tmp.path(), None).expect("repo init");
    let dimensions = embedding.dimensions();
    Arc::new(AppState::new(
        Arc::new(repo),
        "main".to_string(),
        embedding,
        Box::new(InMemoryStore::new(dimensions)),
        AuthProvider::new(),
        health,
        None,
    ))
}

/// Build an [`AppState`] with the deterministic [`StubEmbeddingBackend`],
/// an [`InMemoryStore`], and a fresh temp-dir repo.
///
/// The caller controls which health reporters are marked OK.
pub fn build_stub_state(tmp: &tempfile::TempDir, health: HealthRegistry) -> Arc<AppState> {
    build_state_with_embedding(tmp, health, Box::new(StubEmbeddingBackend))
}

/// Build an [`AppState`] with all health subsystems marked OK.
pub fn build_healthy_state(tmp: &tempfile::TempDir) -> Arc<AppState> {
    let registry = HealthRegistry::new();
    registry.git.report_ok();
    registry.embedding.report_ok();
    registry.vector_index.report_ok();
    build_stub_state(tmp, registry)
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the full axum router (MCP service + health routes) for in-process
/// testing.
///
/// `allowed_hosts` replaces the default allowed-hosts list in
/// [`StreamableHttpServerConfig`]. The default list is
/// `["localhost", "127.0.0.1", "::1"]`.
pub fn build_test_router(state: Arc<AppState>, allowed_hosts: Vec<String>) -> axum::Router {
    let ct = CancellationToken::new();
    let ct_child = ct.child_token();
    // Keep the parent token alive for the lifetime of the router so the child
    // is not immediately cancelled on drop.
    std::mem::forget(ct);

    let state_clone = Arc::clone(&state);

    // Start from the default config and override allowed_hosts.
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = ct_child;
    config.allowed_hosts = allowed_hosts;

    let service = StreamableHttpService::new(
        move || Ok(MemoryServer::new(Arc::clone(&state_clone))),
        BoundedSessionManagerBuilder::new(10).build(),
        config,
    );

    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz_handler))
        .route("/readyz", axum::routing::get(readyz_handler))
        .route("/version", axum::routing::get(version_handler))
        .with_state(Arc::clone(&state))
        .nest_service("/mcp", service)
}
