//! `server/discover` before `initialize` (lacuna/pali#421).
//!
//! Newer MCP clients probe with `server/discover` and fall back to the
//! `initialize` handshake only on HTTP 400, 404 or 405. rmcp 1.8 answered the
//! probe with 422, which those clients treat as fatal. These tests post the
//! probe to the in-process router, outside any session, and then open a normal
//! session on the same router.

mod common;
// This crate uses only part of the harness; the rest would warn as dead code.
#[allow(dead_code)]
#[path = "common/mcp_harness.rs"]
mod mcp_harness;

use std::time::Duration;

use axum::body::{Body, HttpBody as _};
use http::{header, Request, StatusCode};
use mcp_harness::TestServer;
use serde_json::{json, Value};
use tower::ServiceExt as _;

fn loopback_router(tmp: &tempfile::TempDir) -> axum::Router {
    let state = common::build_healthy_state(tmp);
    common::build_test_router(
        state,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ],
    )
}

/// POST `body` to `/mcp` with no session id, as a client's first request,
/// adding `extra_headers` to the usual Host, Content-Type and Accept.
async fn post_first(
    router: &axum::Router,
    extra_headers: &[(&str, &str)],
    body: Value,
) -> http::Response<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Body::from(body.to_string()))
        .expect("static request parts are valid");
    tokio::time::timeout(Duration::from_secs(30), router.clone().oneshot(request))
        .await
        .expect("router answered within 30 s")
        .expect("router call succeeded")
}

/// The first JSON-RPC message in a JSON or SSE body that carries `result`
/// or `error`. An SSE stream need not close after its response, so frames
/// are read only until that message has arrived.
async fn first_reply(response: http::Response<Body>) -> Value {
    let mut body = response.into_body();
    let read = async move {
        let mut text = String::new();
        loop {
            let parsed = serde_json::from_str::<Value>(text.trim()).ok().or_else(|| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                    .find(|message| {
                        message.get("result").is_some() || message.get("error").is_some()
                    })
            });
            if let Some(message) = parsed {
                return message;
            }
            match std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        text.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                Some(Err(e)) => panic!("response body failed: {e}"),
                None => panic!("body ended without a JSON-RPC reply: {text}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("a reply within 30 s")
}

/// The whole body as text, for a failure message (64 KiB at most).
async fn body_text(response: http::Response<Body>) -> String {
    match tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(Err(e)) => format!("<unreadable body: {e}>"),
        Err(_) => "<body not complete within 5 s>".to_string(),
    }
}

/// The statuses after which a discover-first client falls back to
/// `initialize`, plus success.
fn is_answer_a_client_can_use(status: StatusCode) -> bool {
    status.is_success()
        || matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        )
}

/// The probe OMP 19.1.1 sends, headers and body as captured from it, is
/// answered with the server's discovery result. rmcp 3.11 requires the
/// `MCP-Protocol-Version` header to match `_meta`'s protocolVersion, and at
/// 2026-07-28 an `Mcp-Method` header matching the body (SEP-2243); without
/// either it answers 400. Red on rmcp 1.8, which answered 422.
#[tokio::test]
async fn server_discover_before_initialize_is_answered() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let router = loopback_router(&tmp);

    let response = post_first(
        &router,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "server/discover"),
        ],
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": { "roots": {} },
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "omp-coding-agent",
                        "version": "1.0.0",
                    },
                },
            },
        }),
    )
    .await;

    let status = response.status();
    if status != StatusCode::OK {
        panic!(
            "expected 200 for OMP 19's probe, got {status}: {}",
            body_text(response).await
        );
    }
    let reply = first_reply(response).await;
    assert_eq!(reply["id"], json!(1), "reply: {reply}");
    assert!(reply.get("result").is_some(), "reply: {reply}");
}

/// A bare probe with no `_meta` (what a client unsure of the revision may
/// send) is refused with a status that sends the client on to `initialize`,
/// never 422; a normal session then opens on the same router.
#[tokio::test]
async fn bare_server_discover_falls_back_and_initialize_still_succeeds() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let router = loopback_router(&tmp);

    let response = post_first(
        &router,
        &[],
        json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {} }),
    )
    .await;
    let status = response.status();
    assert_ne!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        is_answer_a_client_can_use(status),
        "a discover-first client cannot fall back from {status}"
    );

    let server = TestServer::new(router);
    let client = server.connect().await.expect("initialize after discover");
    let tools = client
        .request("tools/list", json!({}))
        .await
        .expect("tools/list in the new session");
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "tools/list: {tools}"
    );
}
