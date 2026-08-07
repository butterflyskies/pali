//! Integration tests for the health and info HTTP endpoints (`/readyz`, `/version`).
//!
//! All tests use the in-process router (tower oneshot) — no subprocess or port
//! allocation. Stub backends let us control subsystem health directly via the
//! [`HealthRegistry`] without starting the embedding engine.

mod common;

use axum::body::{to_bytes, Body};
use http::Request;
use pali::health::HealthRegistry;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Healthy server: /readyz returns 200 with all subsystems "up".
#[tokio::test]
async fn readyz_healthy_server_returns_200_ready() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = common::build_healthy_state(&tmp);
    let router = common::build_test_router(
        state,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ],
    );

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .header("host", "127.0.0.1")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("service call should not fail");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "healthy server should return 200"
    );

    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("body should be valid JSON");

    assert_eq!(body["status"], "ready");

    for check in &["git_repo", "embedding", "vector_index"] {
        assert_eq!(
            body["checks"][check]["status"], "up",
            "check '{check}' should be 'up', got: {body}"
        );
        assert!(
            body["checks"][check]["reason"].is_null(),
            "check '{check}' should have no reason when up"
        );
    }
}

/// Degraded server: /readyz returns 503 with failing subsystems.
///
/// The [`HealthRegistry`] is the source of truth — we mark subsystems as
/// degraded directly via `report_err`, bypassing any subsystem implementation.
#[tokio::test]
async fn readyz_degraded_server_returns_503_not_ready() {
    let tmp = tempfile::tempdir().expect("temp dir");

    // Build a registry and mark the embedding subsystem as degraded.
    let registry = HealthRegistry::new();
    registry.embedding.report_err("embed failed");

    let state = common::build_stub_state(&tmp, registry);
    let router = common::build_test_router(
        state,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ],
    );

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .header("host", "127.0.0.1")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("service call should not fail");

    assert_eq!(
        resp.status().as_u16(),
        503,
        "degraded server should return 503"
    );

    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("body should be valid JSON");

    assert_eq!(body["status"], "not_ready");

    // Embedding was explicitly marked down with a reason.
    assert_eq!(body["checks"]["embedding"]["status"], "down");
    assert_eq!(body["checks"]["embedding"]["reason"], "embed failed");

    // Git and vector_index were never reported (still "not yet checked").
    assert_eq!(body["checks"]["git_repo"]["status"], "down");
    assert_eq!(body["checks"]["vector_index"]["status"], "down");
}

/// A known vector-mirror gap must keep readiness red even when the embedding
/// worker and vector store most recently reported successful operations.
#[tokio::test]
async fn readyz_incomplete_vector_mirror_returns_503_not_ready() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = common::build_healthy_state(&tmp);
    state.mark_index_mirror_incomplete();
    let router = common::build_test_router(
        state,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ],
    );

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .header("host", "127.0.0.1")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("service call should not fail");

    assert_eq!(resp.status().as_u16(), 503);
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("body should be valid JSON");

    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["checks"]["embedding"]["status"], "up");
    assert_eq!(body["checks"]["vector_index"]["status"], "down");
    assert_eq!(
        body["checks"]["vector_index"]["reason"],
        "mirror incomplete"
    );
}

/// `/version` returns 200 with the crate version from Cargo.toml.
#[tokio::test]
async fn version_returns_cargo_pkg_version() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = common::build_healthy_state(&tmp);
    let router = common::build_test_router(
        state,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ],
    );

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/version")
                .header("host", "127.0.0.1")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("service call should not fail");

    assert_eq!(resp.status().as_u16(), 200);

    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("body should be valid JSON");

    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}
