//! In-process MCP client and the populated-store self-check.
//!
//! Include with `#[path = "common/mcp_harness.rs"] mod mcp_harness;` next to
//! `mod common;`. The client speaks Streamable HTTP to a router built by
//! `common::build_test_router`, so every call crosses the same session layer,
//! header injection and tool dispatch a real client does.
//!
//! An isolation test proves nothing against an empty store: "no memory came
//! back" cannot fail when there is no memory to leak. Such a test must call
//! [`assert_populated_store`] first, as the principal that owns the data.

use std::{
    fmt,
    future::poll_fn,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use axum::body::{Body, HttpBody as _};
use http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt as _;

/// Protocol version offered in `initialize`; the server's reply is echoed
/// back on later requests.
const OFFERED_PROTOCOL_VERSION: &str = "2025-11-25";

/// Upper bound on waiting for one JSON-RPC response. A hang is reported as
/// a transport failure instead of stalling the test run.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A failure below the tool layer: the request never produced a JSON-RPC
/// response (bad status, unreadable body, timeout).
#[derive(Debug)]
pub struct TransportError {
    /// HTTP status, when a response arrived at all.
    pub status: Option<StatusCode>,
    /// What went wrong, including any response body text.
    pub detail: String,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status}: {}", self.detail),
            None => write!(f, "{}", self.detail),
        }
    }
}

/// The outcome of one `tools/call` that reached the server.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolReply {
    /// The tool succeeded; the text of its first content item.
    Success(String),
    /// The tool ran and reported `isError: true`.
    ToolError(String),
    /// The server answered with a JSON-RPC error.
    RpcError {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
}

/// Source of [`TestServer`] ids, unique within the test process.
static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);

/// One in-process server under test: a router plus an identity.
///
/// Every client opened from a `TestServer` carries its id, and so does every
/// [`SelfCheckReceipt`] produced through such a client. That is how a receipt
/// is bound to the store it checked: clients for different principals on the
/// same server share the id, while a client on any other server does not.
/// Two `TestServer`s wrapping clones of one router get different ids, which
/// errs towards reporting the harness broken.
pub struct TestServer {
    router: axum::Router,
    id: u64,
}

impl TestServer {
    /// Wrap `router` with a fresh process-unique id.
    pub fn new(router: axum::Router) -> Self {
        Self {
            router,
            id: NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Open a session with no extra request headers.
    pub async fn connect(&self) -> Result<McpTestClient, TransportError> {
        self.connect_with_headers(HeaderMap::new()).await
    }

    /// Open a session that sends `headers` on every request, including the
    /// `initialize` handshake. A header named here replaces the client's
    /// default for that name (`Host`, `Accept`, `Content-Type`); the session
    /// and protocol-version headers are always the client's own.
    pub async fn connect_with_headers(
        &self,
        headers: HeaderMap,
    ) -> Result<McpTestClient, TransportError> {
        McpTestClient::connect(&self.router, self.id, headers).await
    }
}

/// An initialised MCP session against a [`TestServer`].
///
/// `headers` are sent on every request, so a caller can attach one
/// principal's credentials and open a second client for another principal.
pub struct McpTestClient {
    router: axum::Router,
    server_id: u64,
    headers: HeaderMap,
    session_id: HeaderValue,
    protocol_version: HeaderValue,
    next_id: AtomicU64,
}

impl McpTestClient {
    async fn connect(
        router: &axum::Router,
        server_id: u64,
        headers: HeaderMap,
    ) -> Result<Self, TransportError> {
        let init = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": OFFERED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "pali-test-harness", "version": "0" },
            },
        });
        let response = post(router, &headers, None, init).await?;
        let session_id = response.headers().get("mcp-session-id").cloned();
        // Status is checked first so a refused handshake reports its own
        // status and body rather than a missing session header.
        let reply = read_rpc_response(response, 0).await?;
        let session_id = session_id.ok_or_else(|| TransportError {
            status: Some(StatusCode::OK),
            detail: "initialize response carried no mcp-session-id header".to_string(),
        })?;
        let protocol_version = reply["result"]["protocolVersion"]
            .as_str()
            .and_then(|v| HeaderValue::from_str(v).ok())
            .ok_or_else(|| TransportError {
                status: None,
                detail: format!("initialize reply had no usable protocolVersion: {reply}"),
            })?;

        let client = Self {
            router: router.clone(),
            server_id,
            headers,
            session_id,
            protocol_version,
            next_id: AtomicU64::new(1),
        };
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let response = post(
            &client.router,
            &client.headers,
            Some((&client.session_id, &client.protocol_version)),
            initialized,
        )
        .await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(failed_status(response).await);
        }
        Ok(client)
    }

    /// Call one tool and classify the reply.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolReply, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        let response = post(
            &self.router,
            &self.headers,
            Some((&self.session_id, &self.protocol_version)),
            request,
        )
        .await?;
        let reply = read_rpc_response(response, id).await?;
        classify_tool_reply(&reply)
    }

    /// End the session with `DELETE /mcp`.
    ///
    /// An open session keeps its server handler, and so the whole app state
    /// with its keyword index and writer threads, alive until the process
    /// exits. A test that builds many servers must close its clients, or it
    /// exhausts the thread limit and later keyword indexes come up disabled.
    pub async fn close(self) -> Result<(), TransportError> {
        let mut request = Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1")
            .body(Body::empty())
            .expect("static request parts are valid");
        request.headers_mut().extend(self.headers.clone());
        request
            .headers_mut()
            .insert("mcp-session-id", self.session_id.clone());
        request
            .headers_mut()
            .insert("mcp-protocol-version", self.protocol_version.clone());
        let response = tokio::time::timeout(RESPONSE_TIMEOUT, self.router.oneshot(request))
            .await
            .map_err(|_| TransportError {
                status: None,
                detail: format!("session close gave no response within {RESPONSE_TIMEOUT:?}"),
            })?
            .map_err(|e| TransportError {
                status: None,
                detail: format!("router call failed: {e}"),
            })?;
        if response.status() == StatusCode::ACCEPTED {
            Ok(())
        } else {
            Err(failed_status(response).await)
        }
    }
}

/// Classify a validated `tools/call` response.
///
/// A tool-level error (`isError: true`) stays a [`ToolReply::ToolError`] even
/// without text, since the server did answer at the tool layer. A success
/// without text, or a JSON-RPC error without a code or message, is malformed.
fn classify_tool_reply(reply: &Value) -> Result<ToolReply, TransportError> {
    let malformed = |why: &str| TransportError {
        status: Some(StatusCode::OK),
        detail: format!("{why}: {reply}"),
    };
    if let Some(error) = reply.get("error") {
        let (Some(code), Some(message)) = (error["code"].as_i64(), error["message"].as_str())
        else {
            return Err(malformed("JSON-RPC error lacks a code or message"));
        };
        return Ok(ToolReply::RpcError {
            code,
            message: message.to_string(),
        });
    }
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().map(str::to_string);
    if result["isError"].as_bool().unwrap_or(false) {
        return Ok(ToolReply::ToolError(text.unwrap_or_default()));
    }
    text.map(ToolReply::Success)
        .ok_or_else(|| malformed("tool result has no text content"))
}

async fn post(
    router: &axum::Router,
    headers: &HeaderMap,
    session: Option<(&HeaderValue, &HeaderValue)>,
    body: Value,
) -> Result<http::Response<Body>, TransportError> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .expect("static request parts are valid");
    request.headers_mut().extend(headers.clone());
    if let Some((session_id, protocol_version)) = session {
        request
            .headers_mut()
            .insert("mcp-session-id", session_id.clone());
        request
            .headers_mut()
            .insert("mcp-protocol-version", protocol_version.clone());
    }
    tokio::time::timeout(RESPONSE_TIMEOUT, router.clone().oneshot(request))
        .await
        .map_err(|_| TransportError {
            status: None,
            detail: format!("router gave no response head within {RESPONSE_TIMEOUT:?}"),
        })?
        .map_err(|e| TransportError {
            status: None,
            detail: format!("router call failed: {e}"),
        })
}

async fn failed_status(response: http::Response<Body>) -> TransportError {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|e| format!("<unreadable body: {e}>"));
    TransportError {
        status: Some(status),
        detail: body,
    }
}

/// Read the JSON-RPC response with `id` from a JSON or SSE body.
///
/// A request-wise SSE stream may carry a priming event and notifications
/// before the response, and need not close after it, so events are parsed
/// as they arrive and reading stops at the matching response.
async fn read_rpc_response(
    response: http::Response<Body>,
    id: u64,
) -> Result<Value, TransportError> {
    if response.status() != StatusCode::OK {
        return Err(failed_status(response).await);
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let mut body = response.into_body();
    let read = async move {
        let mut buffer = Vec::new();
        loop {
            if !is_json {
                if let Some(reply) = take_sse_response(&mut buffer, id)? {
                    return Ok(reply);
                }
            }
            match poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        buffer.extend_from_slice(&data);
                    }
                }
                Some(Err(e)) => {
                    return Err(TransportError {
                        status: Some(StatusCode::OK),
                        detail: format!("response body failed: {e}"),
                    })
                }
                None if is_json => {
                    let message: Value =
                        serde_json::from_slice(&buffer).map_err(|e| TransportError {
                            status: Some(StatusCode::OK),
                            detail: format!(
                                "response is not JSON ({e}): {}",
                                String::from_utf8_lossy(&buffer)
                            ),
                        })?;
                    return validate_rpc_response(message, id);
                }
                None => {
                    return Err(TransportError {
                        status: Some(StatusCode::OK),
                        detail: format!(
                            "SSE stream ended without a response to id {id}: {}",
                            String::from_utf8_lossy(&buffer)
                        ),
                    })
                }
            }
        }
    };
    tokio::time::timeout(RESPONSE_TIMEOUT, read)
        .await
        .map_err(|_| TransportError {
            status: Some(StatusCode::OK),
            detail: format!("no response to id {id} within {RESPONSE_TIMEOUT:?}"),
        })?
}

/// Consume complete SSE events from `buffer`, returning the JSON-RPC
/// response to `id` if one of them is it.
///
/// Works on bytes and decodes only whole events, so a multi-byte character
/// split across body frames is never decoded half-received.
fn take_sse_response(buffer: &mut Vec<u8>, id: u64) -> Result<Option<Value>, TransportError> {
    loop {
        let Some((end, terminator)) = find_event_end(buffer) else {
            return Ok(None);
        };
        let raw: Vec<u8> = buffer.drain(..end + terminator).collect();
        let event = String::from_utf8_lossy(&raw[..end]).replace("\r\n", "\n");
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| data.strip_prefix(' ').unwrap_or(data))
            .collect();
        if data.iter().all(|d| d.is_empty()) {
            continue;
        }
        let message: Value =
            serde_json::from_str(&data.join("\n")).map_err(|e| TransportError {
                status: Some(StatusCode::OK),
                detail: format!("SSE data is not JSON ({e}): {event}"),
            })?;
        // Server-to-client requests and notifications share the stream and
        // carry a method; only a method-less message can be our response.
        if message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id)
        {
            return validate_rpc_response(message, id).map(Some);
        }
    }
}

/// Accept `message` only as a well-formed JSON-RPC response to `id`: the
/// matching id and exactly one of `result` or `error`. Anything else is a
/// malformed reply, never a success.
fn validate_rpc_response(message: Value, id: u64) -> Result<Value, TransportError> {
    let malformed = |why: &str| TransportError {
        status: Some(StatusCode::OK),
        detail: format!("malformed JSON-RPC reply to id {id} ({why}): {message}"),
    };
    if message.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(malformed("id does not match"));
    }
    match (message.get("result"), message.get("error")) {
        (Some(_), None) | (None, Some(_)) => Ok(message),
        (Some(_), Some(_)) => Err(malformed("both result and error")),
        (None, None) => Err(malformed("neither result nor error")),
    }
}

/// Offset and length of the first SSE event terminator (a blank line).
fn find_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

// ---------------------------------------------------------------------------
// Populated-store self-check
// ---------------------------------------------------------------------------

/// Content of the decoy the self-check writes beside every seed, as
/// `<seed name>-decoy` in the seed's scope; `list` over that scope therefore
/// counts at least 2 after the check. It shares no tokens with
/// [`StoreSeed::canary`], so a working embedder separates them.
const DECOY_CONTENT: &str = "Quarterly invoice reconciliation spreadsheet, unrelated decoy row.";

/// The memory the self-check writes and then expects to read back.
///
/// `scope` must be in the form the server reports it (`global` or a bare
/// path such as `org/team`), because results are matched on scope and name.
pub struct StoreSeed {
    scope: String,
    name: String,
    content: String,
}

impl StoreSeed {
    /// A seed with caller-chosen scope, name and content.
    pub fn new(
        scope: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            scope: scope.into(),
            name: name.into(),
            content: content.into(),
        }
    }

    /// The standard canary memory in `scope`.
    pub fn canary(scope: impl Into<String>) -> Self {
        Self::new(
            scope,
            "populated-store-canary",
            "Populated-store canary written by the test harness: amber heron lantern quorum.",
        )
    }

    /// The seed's scope.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The seed's memory name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The unrelated memory written beside this seed: `<name>-decoy` in the
    /// seed's scope, so the scope holds at least 2 memories after the check.
    fn decoy(&self) -> Self {
        Self::new(
            self.scope.clone(),
            format!("{}-decoy", self.name),
            DECOY_CONTENT,
        )
    }

    fn is(&self, item: &Value) -> bool {
        item["name"].as_str() == Some(self.name.as_str())
            && item["scope"].as_str() == Some(self.scope.as_str())
    }
}

/// Which step of the self-check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfCheckStage {
    /// `remember` did not store the seed or its decoy.
    Seed,
    /// `list` did not count or name both memories.
    List,
    /// `recall` did not rank each memory first for its own text through the
    /// embedding path.
    Recall,
}

/// The self-check failed: the harness cannot produce trustworthy results.
#[derive(Debug)]
pub struct HarnessBroken {
    /// The step that failed.
    pub stage: SelfCheckStage,
    /// What the server returned at that step.
    pub detail: String,
}

impl fmt::Display for HarnessBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HARNESS BROKEN: populated-store self-check failed at {:?}: {}. The store this run \
             tests against is not demonstrably populated and searchable, so an empty result \
             proves nothing; report this run as broken, not as a pass.",
            self.stage, self.detail
        )
    }
}

/// Proof that [`populated_store_self_check`] passed on one [`TestServer`].
///
/// What a receipt guarantees: the fields are private, so it exists only if
/// the check passed, and [`SelfCheckReceipt::assert_covers`] ties it to the
/// server the check ran on.
///
/// What it does not guarantee, and #378 must enforce itself: that an
/// isolation assertion runs only after the check (nothing here stops a test
/// from asserting first or without a receipt); that a check panicking inside
/// a spawned task fails the test (a dropped `JoinError` swallows it, so await
/// the check on the test's own task); and that the seed is still present
/// when the assertion runs. Isolation helpers should take `(&McpTestClient,
/// &SelfCheckReceipt)` and call `assert_covers` before asserting anything.
#[derive(Debug)]
#[must_use = "an isolation test must hold the receipt it asserts under"]
pub struct SelfCheckReceipt {
    server_id: u64,
    scope: String,
    list_count: u64,
}

impl SelfCheckReceipt {
    /// Whether `client` talks to the server this receipt checked.
    pub fn covers(&self, client: &McpTestClient) -> bool {
        self.server_id == client.server_id
    }

    /// Panic with a `HARNESS BROKEN` message unless `client` talks to the
    /// server this receipt checked.
    pub fn assert_covers(&self, client: &McpTestClient) {
        assert!(
            self.covers(client),
            "HARNESS BROKEN: the self-check receipt is for test server {} but the client \
             talks to test server {}; the populated store this run asserts against is \
             unverified, so report this run as broken, not as a pass.",
            self.server_id,
            client.server_id
        );
    }

    /// The scope the check seeded and verified.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// `count` from `list` over the seed's scope.
    pub fn list_count(&self) -> u64 {
        self.list_count
    }
}

/// Seed a memory and an unrelated decoy as `client`, then confirm the store
/// holds and can discriminate them.
///
/// Passes only if:
/// - `remember` succeeds for both;
/// - `list` over the seed's scope counts more than zero memories and names
///   both, matched on scope and name;
/// - `recall` with each memory's own text ranks that memory first as a
///   `"both"` match (embedding and keyword paths), at an embedding distance
///   of at most [`SELF_DISTANCE_MAX`], with the other memory at least
///   [`SEPARATION_MIN`] further away.
///
/// The recall conditions reject a dead embedder hidden behind keyword
/// search, a dead keyword index hidden behind embeddings, and random,
/// constant or jittered embedders, which cannot meet the distance bounds.
/// They assume a deterministic embedder and a seed that shares few tokens
/// with the fixed decoy text.
pub async fn populated_store_self_check(
    client: &McpTestClient,
    seed: &StoreSeed,
) -> Result<SelfCheckReceipt, HarnessBroken> {
    let broken = |stage, detail: String| HarnessBroken { stage, detail };
    let decoy = seed.decoy();

    for memory in [seed, &decoy] {
        let remembered = client
            .call_tool(
                "remember",
                json!({ "name": memory.name, "content": memory.content, "scope": memory.scope }),
            )
            .await
            .map_err(|e| broken(SelfCheckStage::Seed, format!("transport failure: {e}")))?;
        if !matches!(remembered, ToolReply::Success(_)) {
            return Err(broken(
                SelfCheckStage::Seed,
                format!("remember {:?} did not succeed: {remembered:?}", memory.name),
            ));
        }
    }

    let listed = success_json(
        client
            .call_tool("list", json!({ "scope": seed.scope }))
            .await,
    )
    .map_err(|detail| broken(SelfCheckStage::List, detail))?;
    let list_count = listed["count"].as_u64().unwrap_or(0);
    let listed_items = listed["memories"].as_array().cloned().unwrap_or_default();
    for memory in [seed, &decoy] {
        if list_count == 0 || !listed_items.iter().any(|item| memory.is(item)) {
            return Err(broken(
                SelfCheckStage::List,
                format!(
                    "expected count > 0 including {:?} in scope {:?}; got count {list_count}, \
                     memories {}",
                    memory.name,
                    memory.scope,
                    labels(&listed["memories"])
                ),
            ));
        }
    }

    recall_ranks_first(client, seed, &decoy)
        .await
        .map_err(|detail| broken(SelfCheckStage::Recall, detail))?;
    recall_ranks_first(client, &decoy, seed)
        .await
        .map_err(|detail| broken(SelfCheckStage::Recall, detail))?;

    Ok(SelfCheckReceipt {
        server_id: client.server_id,
        scope: seed.scope.clone(),
        list_count,
    })
}

/// Largest embedding distance accepted between a memory and its own text.
/// A deterministic embedder maps identical text to an identical vector, so
/// the true distance is zero; this allows only `f32` rounding.
const SELF_DISTANCE_MAX: f64 = 1e-6;

/// Smallest gap required between the other memory's distance and the
/// target's for the target's own text. The canary and decoy share no tokens,
/// so the hash embedder puts them about 1.0 apart; random or near-constant
/// embedders land far inside this margin.
const SEPARATION_MIN: f64 = 0.5;

/// Recall with `target`'s own text; require `target` first, found by both
/// the embedding and the keyword path, at near-zero distance, and `other`
/// at least [`SEPARATION_MIN`] further away.
async fn recall_ranks_first(
    client: &McpTestClient,
    target: &StoreSeed,
    other: &StoreSeed,
) -> Result<(), String> {
    let recalled = success_json(
        client
            .call_tool(
                "recall",
                json!({ "query": target.content, "scope": target.scope }),
            )
            .await,
    )?;
    let results = recalled["results"].as_array().cloned().unwrap_or_default();
    let got = labels(&recalled["results"]);
    let Some(first) = results.first().filter(|hit| target.is(hit)) else {
        return Err(format!(
            "recall for {:?}'s own text did not rank it first; got {got}",
            target.name
        ));
    };
    // "both": a recall served by one path alone (a dead embedder behind
    // keyword search, or a skipped or failing keyword index behind
    // embeddings) would hide any leak that travels by the other path.
    let match_type = first["match_type"].as_str().unwrap_or_default();
    if match_type != "both" {
        return Err(format!(
            "recall found {:?} only as {match_type:?}; both the embedding and the keyword path \
             must contribute",
            target.name
        ));
    }
    // On a store holding more than the seed, the other seed can fall out of
    // the results, or come back by keyword only with no distance. Then some
    // other hit the embedding arm did return must stand at least
    // SEPARATION_MIN beyond the target: a constant embedder, with every
    // distance equal, still fails, and so does a result list with no other
    // embedding hit at all.
    let other_hit = results.iter().find(|hit| other.is(hit));
    let other_distance = other_hit.and_then(semantic_distance).or_else(|| {
        results
            .iter()
            .filter(|hit| !target.is(hit))
            .filter_map(semantic_distance)
            .reduce(f64::max)
    });
    match (semantic_distance(first), other_distance) {
        (Some(t), Some(o)) if t <= SELF_DISTANCE_MAX && o - t >= SEPARATION_MIN => Ok(()),
        (t, o) => Err(format!(
            "embedder does not separate {:?} from {:?} for {:?}'s own text: own distance {t:?} \
             (must be <= {SELF_DISTANCE_MAX:e}), other distance {o:?} (must exceed it by >= \
             {SEPARATION_MIN}); a random, constant or non-deterministic embedder looks like this",
            target.name, other.name, target.name
        )),
    }
}

/// The hit's semantic distance; `None` for a lexical-only hit, whose
/// distance is the negative sentinel.
fn semantic_distance(hit: &Value) -> Option<f64> {
    hit["distance"].as_f64().filter(|d| *d >= 0.0)
}

/// Run [`populated_store_self_check`] and panic with a `HARNESS BROKEN`
/// message if it fails. Call this before any isolation assertion.
pub async fn assert_populated_store(client: &McpTestClient, seed: &StoreSeed) -> SelfCheckReceipt {
    match populated_store_self_check(client, seed).await {
        Ok(receipt) => receipt,
        Err(broken) => panic!("{broken}"),
    }
}

fn success_json(reply: Result<ToolReply, TransportError>) -> Result<Value, String> {
    match reply {
        Ok(ToolReply::Success(text)) => {
            serde_json::from_str(&text).map_err(|e| format!("reply is not JSON ({e}): {text}"))
        }
        Ok(other) => Err(format!("tool did not succeed: {other:?}")),
        Err(e) => Err(format!("transport failure: {e}")),
    }
}

/// `scope/name` labels for a result array, for failure messages.
fn labels(items: &Value) -> String {
    let labels: Vec<String> = items
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            format!(
                "{}/{}",
                item["scope"].as_str().unwrap_or("?"),
                item["name"].as_str().unwrap_or("?")
            )
        })
        .collect();
    format!("{labels:?}")
}

#[cfg(test)]
mod tests {
    use super::{classify_tool_reply, validate_rpc_response, ToolReply};
    use serde_json::json;

    // Red if a reply without the matching id or with neither/both of
    // result and error is accepted; `{}` must never classify as success.
    #[test]
    fn rpc_reply_validation_rejects_malformed_replies() {
        for malformed in [
            json!({}),
            json!({ "jsonrpc": "2.0", "id": 7 }),
            json!({ "jsonrpc": "2.0", "id": 8, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 7, "result": {}, "error": {} }),
            json!({ "jsonrpc": "2.0", "result": {} }),
        ] {
            let error = validate_rpc_response(malformed.clone(), 7)
                .expect_err(&format!("{malformed} must be rejected"));
            assert!(error.detail.contains("malformed"), "{}", error.detail);
        }
        for valid in [
            json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -1, "message": "x" } }),
        ] {
            validate_rpc_response(valid, 7).expect("well-formed reply");
        }
    }

    // Red if a tool-level error without text is mistaken for a transport
    // failure, or a success without text is accepted as Success("").
    #[test]
    fn tool_reply_classification_keeps_layers_apart() {
        let classify = |reply| classify_tool_reply(&reply);
        assert_eq!(
            classify(json!({ "id": 1, "result": { "isError": true } })).expect("tool layer"),
            ToolReply::ToolError(String::new())
        );
        assert_eq!(
            classify(json!({ "id": 1, "result": {
                "isError": true, "content": [{ "type": "text", "text": "denied" }] } }))
            .expect("tool layer"),
            ToolReply::ToolError("denied".to_string())
        );
        assert_eq!(
            classify(json!({ "id": 1, "result": {
                "content": [{ "type": "text", "text": "ok" }] } }))
            .expect("success"),
            ToolReply::Success("ok".to_string())
        );
        assert_eq!(
            classify(json!({ "id": 1, "error": { "code": -32602, "message": "bad" } }))
                .expect("rpc layer"),
            ToolReply::RpcError {
                code: -32602,
                message: "bad".to_string()
            }
        );
        for malformed in [
            json!({ "id": 1, "result": {} }),
            json!({ "id": 1, "result": { "isError": false } }),
            json!({ "id": 1, "error": { "message": "no code" } }),
        ] {
            classify(malformed.clone()).expect_err(&format!("{malformed} is malformed"));
        }
    }
}
