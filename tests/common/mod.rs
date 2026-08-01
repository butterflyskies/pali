//! Shared test helpers for in-process integration tests.
//!
//! Builds the full service stack (StreamableHttpService + axum routes) using
//! stub backends so tests can call via `tower::ServiceExt::oneshot` without
//! spawning a subprocess or binding to a real port.

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
// Stub embedding backend
// ---------------------------------------------------------------------------

/// A no-op embedding backend — returns empty vectors for any input.
///
/// Health is controlled via the [`HealthRegistry`], not this backend.
pub struct StubEmbeddingBackend;

#[async_trait]
impl EmbeddingBackend for StubEmbeddingBackend {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(vec![])
    }

    fn dimensions(&self) -> usize {
        4
    }
}

// ---------------------------------------------------------------------------
// AppState builder
// ---------------------------------------------------------------------------

/// Build an [`AppState`] with stub backends and a fresh temp-dir repo.
///
/// The caller controls which health reporters are marked OK.
pub fn build_stub_state(tmp: &tempfile::TempDir, health: HealthRegistry) -> Arc<AppState> {
    let repo = MemoryRepo::init_or_open(tmp.path(), None).expect("repo init");
    Arc::new(AppState::new(
        Arc::new(repo),
        "main".to_string(),
        Box::new(StubEmbeddingBackend),
        Box::new(InMemoryStore::new(4)),
        AuthProvider::new(),
        health,
        None,
    ))
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
