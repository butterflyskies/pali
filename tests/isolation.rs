//! Two-principal isolation harness (#378): what principal B can see or
//! change of principal A's memory, through every route pali serves.
//!
//! pali has no inbound client authentication today (`docs/security.md`):
//! scope is a tool argument, so B reads A's memory. The tests marked
//! `#[ignore]` say so. Each fails today, asserting on the name or word that
//! leaked; run them with `cargo test --test isolation -- --ignored`. They
//! are ignored only so the suite stays green until auth lands (#379, #383;
//! the bearer guard is janiform#24); the auth work un-ignores them. The
//! tests without `#[ignore]` pass today and must keep passing.
//!
//! The principals are Ottoline (A) and Tamsin (B) from the access corpus
//! (#409). No intended grant there gives Tamsin `ottoline-private`, and its
//! sentinel memory has a name and a phrase nothing else in the corpus uses. Each
//! principal sends a static bearer token. That token is a test fixture
//! only, not a login mode: no verifier accepts it yet, and the auth work
//! supplies one behind its seam that maps these tokens to principals.
//!
//! Every denial here has a control: A's identical call succeeds against the
//! same populated store first, and B's denial must be a policy answer from
//! the server, not a transport failure. The store is checked as populated
//! (`assert_populated_store`) before any isolation assertion, because "no
//! memory came back" proves nothing about an empty store.
//!
//! Scenario ids are from Callisto's contract (pali#115 comment 9662).

mod common;
// This crate uses only part of the harness; the rest would warn as dead code.
#[allow(dead_code)]
#[path = "common/mcp_harness.rs"]
mod mcp_harness;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use axum::body::Body;
use http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use mcp_harness::{
    assert_populated_store, McpTestClient, StoreSeed, TestServer, ToolReply, TransportError,
};
use pali::{
    auth::AuthProvider,
    health::HealthRegistry,
    index::InMemoryStore,
    recall_log::RecallLog,
    repo::MemoryRepo,
    search::spawn_lexical_repair_for_router,
    types::{AppState, Memory},
};
use serde_json::{json, Value};
use tower::ServiceExt as _;

/// Principal A's private scope in the access corpus.
const A_SCOPE: &str = "ottoline-private";
/// A's sentinel memory, in [`A_SCOPE`].
const A_SENTINEL: &str = "scoped-sentinel-ottoline";
/// The sentinel word. The root sentinel, which B may read, carries it too,
/// so it is a query, never a leak test: see [`A_SENTINEL_PHRASE`].
const A_SENTINEL_WORD: &str = "marrowfat";
/// A phrase only [`A_SENTINEL`]'s body contains.
const A_SENTINEL_PHRASE: &str = "lives in a single private scope";
/// Principal B's own private scope.
const B_SCOPE: &str = "tamsin-private";
/// A memory the corpus holds in B's scope, which B may always read.
const B_OWN: &str = "choir-audition-nerves";

const A_TOKEN: &str = "Bearer fixture-principal-ottoline";
const B_TOKEN: &str = "Bearer fixture-principal-tamsin";

/// Every HTTP route pali's router serves (`src/main.rs`). The
/// `routes_in_main_are_the_ones_swept` test fails if the router gains or
/// loses a `.route(` (on one line or across several), or gains a `.merge`,
/// `.nest`, `.route_service`, `.fallback` or a second `.nest_service`, so a
/// new route cannot skip this sweep.
const HTTP_ROUTES: &[&str] = &["/healthz", "/readyz", "/version"];

/// Routes left public on purpose: liveness, readiness and version. They are
/// named here so that being public is a decision, not an accident (A01).
const PUBLIC_ROUTES: &[&str] = &["/healthz", "/readyz", "/version"];

/// Every tool `tools/list` offers today. `tools_list_is_the_set_swept`
/// fails if a tool is added or removed.
const TOOLS: &[&str] = &[
    "batch_mark_applied",
    "edit",
    "forget",
    "list",
    "mark_applied",
    "move",
    "note",
    "read",
    "recall",
    "recall_stats",
    "remember",
    "sync",
];

/// MCP methods outside `tools/call` that a client can send.
const OTHER_MCP_METHODS: &[&str] = &[
    "tools/list",
    "ping",
    "completion/complete",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
    "logging/setLevel",
];

fn loopback_hosts() -> Vec<String> {
    vec!["127.0.0.1".to_string(), "localhost".to_string()]
}

fn bearer(token: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static(token));
    headers
}

/// A server over the access corpus.
struct World {
    _repo_dir: tempfile::TempDir,
    _recall_log_dir: tempfile::TempDir,
    state: Arc<AppState>,
    server: TestServer,
}

/// The world every test but A14 runs in. When inbound auth lands, this is
/// the one that gains an issuer and audience, and the verifier that maps
/// [`A_TOKEN`] and [`B_TOKEN`] to their principals.
///
/// The router is `common::build_test_router`, a copy of the one `pali serve`
/// builds in `src/main.rs`, and `routes_in_main_are_the_ones_swept` compares
/// route paths only. A bearer layer added to the copy alone would turn these
/// tests green with `pali serve` still open: the auth work must build this
/// world from the builder production serves.
async fn world() -> World {
    build_world().await
}

/// A world with no inbound-auth configuration at all: no issuer, no
/// audience, no verifier. pali has no such settings today, so this is built
/// exactly as [`world`] is; it is kept apart so that configuring [`world`]
/// for auth cannot change what A14 tests.
async fn unconfigured_world() -> World {
    build_world().await
}

async fn build_world() -> World {
    let repo_dir = tempfile::tempdir().expect("temp dir");
    let recall_log_dir = tempfile::tempdir().expect("temp dir");
    let state = seeded_state(&repo_dir, &recall_log_dir, &common::load_access_corpus()).await;
    let router = common::build_test_router(Arc::clone(&state), loopback_hosts());
    World {
        _repo_dir: repo_dir,
        _recall_log_dir: recall_log_dir,
        state,
        server: TestServer::new(router),
    }
}

/// A seeded state with a recall log, as `pali serve` opens by
/// default (`src/main.rs`): without one, `recall_stats` only ever answers
/// "recall log is not enabled" and A12 could not see feedback leak through
/// it. The log sits outside the repository directory, as it does in a real
/// start.
async fn seeded_state(
    repo_dir: &tempfile::TempDir,
    recall_log_dir: &tempfile::TempDir,
    memories: &[Memory],
) -> Arc<AppState> {
    let health = HealthRegistry::new();
    health.git.report_ok();
    health.embedding.report_ok();
    health.vector_index.report_ok();
    let repo = MemoryRepo::init_or_open(repo_dir.path(), None).expect("repo init");
    let recall_log = RecallLog::open(
        &recall_log_dir.path().join("recall_log.sqlite"),
        Duration::from_secs(5),
    )
    .expect("open recall log");
    let state = Arc::new(AppState::new(
        Arc::new(repo),
        "main".to_string(),
        Box::new(common::StubEmbeddingBackend),
        Box::new(InMemoryStore::new(common::HASH_EMBEDDING_DIMS)),
        AuthProvider::new(),
        health,
        Some(Arc::new(recall_log)),
    ));
    common::seed_and_index(&state, memories).await;
    state
}

impl World {
    async fn connect(&self, token: &'static str) -> McpTestClient {
        self.server
            .connect_with_headers(bearer(token))
            .await
            .unwrap_or_else(|e| panic!("HARNESS BROKEN: {token} could not connect: {e}"))
    }

    /// A's client, after A has proved the store populated in A's scope.
    async fn owner(&self) -> McpTestClient {
        let a = self.connect(A_TOKEN).await;
        // The receipt itself is not needed: a failed check panics.
        let _receipt = assert_populated_store(&a, &StoreSeed::canary(A_SCOPE)).await;
        a
    }

    /// B's client, after B has read a memory of its own. Without this a
    /// token mapped to a principal with no grants at all would pass every
    /// "B is refused" assertion: the read pins B as the owner of [`B_SCOPE`].
    async fn other(&self) -> McpTestClient {
        let b = self.connect(B_TOKEN).await;
        let own = call(&b, "read", json!({ "name": B_OWN, "scope": B_SCOPE })).await;
        assert!(
            matches!(own, ToolReply::Success(_)),
            "HARNESS BROKEN: B could not read its own {B_SCOPE}/{B_OWN}: {own:?}"
        );
        b
    }

    /// `(scope, name, body)` for every memory in the repository.
    async fn truth(&self) -> BTreeSet<(String, String, String)> {
        self.state
            .repo
            .list_memories(None)
            .await
            .expect("list repository truth")
            .into_iter()
            .map(|m| (m.metadata.scope.to_string(), m.name.to_string(), m.content))
            .collect()
    }
}

/// Call a tool and require an answer from the server. An HTTP 403 is a
/// policy denial: the MCP authorization spec (2026-07-28, as 2025-06-18)
/// answers a token without the needed scope with 403 `insufficient_scope`,
/// so it stands as a refusal here. Any other transport failure is a broken
/// harness, not a policy denial.
async fn call(client: &McpTestClient, tool: &str, arguments: Value) -> ToolReply {
    if tool == "list" {
        return list_every_page(client, arguments).await;
    }
    call_once(client, tool, arguments).await
}

/// `list` caps a page at 100, and the corpus holds more than that across
/// scopes: walk `next_cursor` and join the pages, so a probe sees the whole
/// set. The first page that is not a success is returned as it is.
async fn list_every_page(client: &McpTestClient, arguments: Value) -> ToolReply {
    let mut pages = Vec::new();
    let mut args = arguments;
    for _ in 0..20 {
        let reply = call_once(client, "list", args.clone()).await;
        let ToolReply::Success(page) = &reply else {
            return reply;
        };
        let next = serde_json::from_str::<Value>(page)
            .ok()
            .and_then(|v| v["next_cursor"].as_str().map(str::to_string));
        pages.push(page.clone());
        match next {
            Some(cursor) => args["cursor"] = Value::from(cursor),
            None => return ToolReply::Success(pages.join("\n")),
        }
    }
    panic!("HARNESS BROKEN: list did not end within 20 pages");
}

async fn call_once(client: &McpTestClient, tool: &str, arguments: Value) -> ToolReply {
    match client.call_tool(tool, arguments.clone()).await {
        Ok(reply) => reply,
        Err(e) if e.status == Some(StatusCode::FORBIDDEN) => {
            ToolReply::ToolError(format!("HTTP 403: {}", e.detail))
        }
        Err(e) => panic!("HARNESS BROKEN: {tool} {arguments} got no answer: {e}"),
    }
}

fn text(reply: &ToolReply) -> &str {
    match reply {
        ToolReply::Success(t) | ToolReply::ToolError(t) => t,
        ToolReply::RpcError { message, .. } => message,
    }
}

fn leaks(reply: &ToolReply) -> bool {
    let t = text(reply);
    t.contains(A_SENTINEL) || t.contains(A_SENTINEL_PHRASE)
}

/// Whether a reply to a `read` of [`A_SENTINEL`] gives the memory away. The
/// caller sent the name, and a refusal may repeat it (`memory not found:
/// <name>`, the very answer A06 asks for), so the name alone is no leak
/// here: a success is, and so is the body's phrase in any reply.
fn read_leaks(reply: &ToolReply) -> bool {
    matches!(reply, ToolReply::Success(_)) || text(reply).contains(A_SENTINEL_PHRASE)
}

/// [`read_leaks`] for `read`, [`leaks`] for every other tool.
fn probe_leaks(tool: &str, reply: &ToolReply) -> bool {
    if tool == "read" {
        read_leaks(reply)
    } else {
        leaks(reply)
    }
}

/// Remove the fields that change on every call, so two observations of the
/// same state compare equal byte for byte.
fn stable(reply: &ToolReply) -> String {
    fn scrub(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for key in ["recall_id", "id", "created_at", "updated_at", "_meta"] {
                    map.remove(key);
                }
                map.values_mut().for_each(scrub);
            }
            Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    let (kind, body) = match reply {
        ToolReply::Success(t) => ("ok", t.as_str()),
        ToolReply::ToolError(t) => ("tool-error", t.as_str()),
        ToolReply::RpcError { message, .. } => ("rpc-error", message.as_str()),
    };
    match serde_json::from_str::<Value>(body) {
        Ok(mut value) => {
            scrub(&mut value);
            format!("{kind} {value}")
        }
        Err(_) => format!("{kind} {body}"),
    }
}

/// The calls through which B could read A's sentinel today: browse and
/// search with every scope form (omitted, `all`, A's scope named), and a
/// direct read, plain and straddled. A straddled read answers on a path of
/// its own in `src/server.rs`, local memory included, so a guard on the
/// plain path alone would leave it open.
fn read_probes() -> Vec<(&'static str, Value)> {
    vec![
        ("list", json!({ "scope": "all", "limit": 100 })),
        ("list", json!({ "scope": A_SCOPE, "limit": 100 })),
        (
            "recall",
            json!({ "query": A_SENTINEL_WORD, "scope": "all" }),
        ),
        (
            "recall",
            json!({ "query": A_SENTINEL_WORD, "scope": A_SCOPE }),
        ),
        ("recall", json!({ "query": A_SENTINEL_WORD })),
        ("read", json!({ "name": A_SENTINEL, "scope": A_SCOPE })),
        (
            "read",
            json!({ "name": A_SENTINEL, "scope": A_SCOPE, "straddle": true }),
        ),
    ]
}

// ---------------------------------------------------------------------------
// A01, A14: no credential, and no auth configured
// ---------------------------------------------------------------------------

// A01. Red today: `initialize` with no credential opens a session, and the
// session then lists A's private memory.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a01_mcp_refuses_a_session_with_no_credential() {
    let w = world().await;
    let a = w.owner().await;
    // Control: the sentinel is there for a principal who may see it.
    assert!(leaks(
        &call(&a, "list", json!({ "scope": "all", "limit": 100 })).await
    ));

    match w.server.connect().await {
        Err(e) if e.status == Some(StatusCode::UNAUTHORIZED) => {}
        Err(e) => panic!("A01: refused, but not as 401 (a dead endpoint is not a denial): {e}"),
        Ok(anon) => {
            let reply = call(&anon, "list", json!({ "scope": "all", "limit": 100 })).await;
            panic!(
                "A01: initialize with no credential opened a session; list(all) then \
                 returned {A_SENTINEL}: {}",
                leaks(&reply)
            );
        }
    }
}

// A01. Red today: a POST to /mcp with no credential and no session is
// turned away for the missing session, not the missing credential. Every
// method must meet the credential check first.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a01_every_mcp_method_needs_a_credential_before_a_session() {
    let w = world().await;
    let mut wrong = Vec::new();
    let methods = OTHER_MCP_METHODS
        .iter()
        .map(|m| (m.to_string(), json!({})))
        .chain(TOOLS.iter().map(|t| {
            (
                "tools/call".to_string(),
                json!({ "name": t, "arguments": {} }),
            )
        }));
    for (method, params) in methods {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let (status, headers, _) =
            raw_body(&w, "POST", "/mcp", HeaderMap::new(), body.to_string()).await;
        // The spec's 401 names where the client finds the authorization
        // server: `WWW-Authenticate: Bearer resource_metadata="…"`.
        let challenge = headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if status != StatusCode::UNAUTHORIZED
            || !challenge.starts_with("Bearer ")
            || !challenge.contains("resource_metadata=")
        {
            wrong.push(format!(
                "{method} {params}: {status}, challenge {challenge:?}"
            ));
        }
    }
    // The stream and the session end are not JSON-RPC methods, and meet the
    // same check.
    for method in ["GET", "DELETE"] {
        let (status, headers, _) =
            raw_body(&w, method, "/mcp", HeaderMap::new(), String::new()).await;
        let challenge = headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if status != StatusCode::UNAUTHORIZED
            || !challenge.starts_with("Bearer ")
            || !challenge.contains("resource_metadata=")
        {
            wrong.push(format!("{method} /mcp: {status}, challenge {challenge:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "A01: not refused as 401 with no credential:\n{}",
        wrong.join("\n")
    );
}

// A14. Red today: pali starts with no issuer or audience configured, and a
// bearer token it has no way to verify is accepted. Missing auth
// configuration must refuse, not mean "auth off".
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a14_no_issuer_configured_refuses_a_presented_token() {
    let w = unconfigured_world().await;
    // Control from the repository, not through a session: in this world no
    // token may open one, A's included.
    assert!(
        w.truth().await.iter().any(|(scope, name, body)| {
            scope == A_SCOPE && name == A_SENTINEL && body.contains(A_SENTINEL_PHRASE)
        }),
        "HARNESS BROKEN: the unconfigured world does not hold {A_SCOPE}/{A_SENTINEL}"
    );

    let mut wrong = Vec::new();
    for token in [A_TOKEN, B_TOKEN] {
        match w.server.connect_with_headers(bearer(token)).await {
            Err(e) if e.status == Some(StatusCode::UNAUTHORIZED) => {}
            Err(e) => wrong.push(format!("{token}: refused, but not as 401: {e}")),
            Ok(client) => {
                let reply = call(
                    &client,
                    "read",
                    json!({ "name": A_SENTINEL, "scope": A_SCOPE }),
                )
                .await;
                wrong.push(format!(
                    "{token}: an unverifiable token opened a session and read {A_SENTINEL} \
                     (leaked: {})",
                    read_leaks(&reply)
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "A14: with no issuer configured:\n{}",
        wrong.join("\n")
    );
}

// ---------------------------------------------------------------------------
// A03, A05, A06: B reading A's memory
// ---------------------------------------------------------------------------

// A05, A06. Red today: every read probe returns A's sentinel to B.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a05_a06_b_never_sees_a_private_memory() {
    let w = world().await;
    let a = w.owner().await;
    let b = w.other().await;

    let mut leaked = Vec::new();
    for (tool, args) in read_probes() {
        // Control first: A's identical call returns the sentinel. Recall
        // with scope omitted is root-only, so it cannot reach A's scope for
        // A either; it stays in the list as a probe of B, without a control.
        let root_only = tool == "recall" && args.get("scope").is_none();
        if !root_only {
            let control = call(&a, tool, args.clone()).await;
            assert!(
                leaks(&control) && probe_leaks(tool, &control),
                "HARNESS BROKEN: A's own {tool} {args} did not return the sentinel: {control:?}"
            );
        }
        let reply = call(&b, tool, args.clone()).await;
        if probe_leaks(tool, &reply) {
            leaked.push(format!("{tool} {args}"));
        }
    }
    assert!(
        leaked.is_empty(),
        "A05/A06: B saw {A_SENTINEL} through:\n{}",
        leaked.join("\n")
    );
}

// A06. Red today: B's read of A's memory succeeds. When it is refused, the
// refusal must look the same as a read of a name that does not exist, so it
// does not confirm that A's memory is there.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a06_b_read_of_a_private_memory_matches_a_missing_name() {
    let w = world().await;
    let a = w.owner().await;
    let b = w.other().await;
    let args = json!({ "name": A_SENTINEL, "scope": A_SCOPE });
    assert!(matches!(
        call(&a, "read", args.clone()).await,
        ToolReply::Success(_)
    ));

    let forbidden = call(&b, "read", args).await;
    let missing_name = "no-such-memory-anywhere";
    let missing = call(
        &b,
        "read",
        json!({ "name": missing_name, "scope": A_SCOPE }),
    )
    .await;

    assert!(
        !read_leaks(&forbidden),
        "A06: B read {A_SENTINEL}: {forbidden:?}"
    );
    assert_eq!(
        stable(&forbidden).replace(A_SENTINEL, "<name>"),
        stable(&missing).replace(missing_name, "<name>"),
        "A06: B can tell A's memory exists from how the refusal differs from a missing name"
    );
}

// A03, sequential: interleaved A and B calls on one server each answer for
// their own caller. Red today: B's call in the middle returns A's sentinel.
// The concurrent schedule is still owed.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a03_interleaved_calls_keep_their_own_principal() {
    let w = world().await;
    let a = w.owner().await;
    let b = w.other().await;
    let args = json!({ "scope": "all", "limit": 100 });

    let first = call(&a, "list", args.clone()).await;
    let middle = call(&b, "list", args.clone()).await;
    let last = call(&a, "list", args).await;

    assert!(leaks(&first), "control: A's first list lacks the sentinel");
    assert!(
        leaks(&last),
        "A03: A's list after B's lost the sentinel: {last:?}"
    );
    assert!(
        !leaks(&middle),
        "A03: B's list between A's returned {A_SENTINEL}"
    );
}

// ---------------------------------------------------------------------------
// A04: sessions
// ---------------------------------------------------------------------------

/// Whether a transport failure is the server refusing B's use of A's
/// session: 401 or 403 for the credential, or 404, the answer the MCP
/// transport gives for a session id it does not hold (here, not for B).
/// Anything else, a 400, a 5xx, a timeout or no response, is no answer.
fn refuses_session(e: &TransportError) -> bool {
    e.status.is_some_and(|status| {
        [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ]
        .contains(&status)
    })
}

// A04. Red today: a session id is enough. B, presenting its own credential
// with A's session id, can call tools in A's session, open its stream, ask
// for a replay of it, and end it.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a04_b_cannot_use_or_end_a_session_by_its_id() {
    let w = world().await;
    let a = w.owner().await;
    let b_in_a = a.borrow_session(bearer(B_TOKEN));

    let used = b_in_a
        .call_tool("list", json!({ "scope": "all", "limit": 100 }))
        .await;
    // The stream side. rmcp resumes a stream on GET with `Last-Event-ID`
    // and keeps a finished request's events for a late resume, so a check on
    // POST and DELETE alone would leave A's answers replayable by B. Any
    // event id will do: the server opens a stream (an empty one for an id it
    // does not know) before it would refuse, and the refusal is what is
    // asked for.
    let listened = b_in_a.open_stream(None).await;
    let replayed = b_in_a.open_stream(Some("0/0")).await;
    // The owner's control for the two stream probes, taken before B's
    // DELETE: after it, a failure here would be the DELETE's doing, which
    // `ended` and `after` already report.
    let a_listens = a.open_stream(None).await;
    let ended = a.borrow_session(bearer(B_TOKEN)).close().await;
    let after = a.call_tool("list", json!({ "scope": A_SCOPE })).await;

    let mut wrong = Vec::new();
    // Any reply, a tool error included, means B's request ran in A's
    // session; only an HTTP refusal keeps B out of it.
    match &used {
        Ok(reply) => wrong.push(format!(
            "B called list in A's session and got an answer (sentinel leaked: {})",
            leaks(reply)
        )),
        Err(e) if refuses_session(e) => {}
        Err(e) => panic!("HARNESS BROKEN: A04: B's call in A's session got no answer: {e}"),
    }
    for (what, got) in [
        ("opened A's session stream with GET /mcp", &listened),
        (
            "asked A's session for a replay with GET /mcp and Last-Event-ID",
            &replayed,
        ),
    ] {
        match got {
            Ok(status) => wrong.push(format!("B {what} and got {status}")),
            Err(e) if refuses_session(e) => {}
            Err(e) => panic!("HARNESS BROKEN: A04: B {what} and got no answer: {e}"),
        }
    }
    match &ended {
        Ok(()) => wrong.push("B ended A's session with DELETE /mcp".to_string()),
        Err(e) if refuses_session(e) => {}
        Err(e) => panic!("HARNESS BROKEN: A04: B's DELETE of A's session got no answer: {e}"),
    }
    if !matches!(after, Ok(ToolReply::Success(_))) {
        wrong.push(format!(
            "A's session no longer works after B's attempts: {after:?}"
        ));
    }
    // Control for the stream probes: the owner may open it.
    if let Err(e) = &a_listens {
        wrong.push(format!("A could not open its own session stream: {e}"));
    }
    assert!(wrong.is_empty(), "A04:\n{}", wrong.join("\n"));
}

// ---------------------------------------------------------------------------
// A07: B's writes to A's memory
// ---------------------------------------------------------------------------

/// Who proves a write probe is a real write: the principal whose identical
/// call must succeed on a store of its own.
#[derive(Clone, Copy)]
enum Control {
    /// A's identical call succeeds: the write is one A may make.
    Owner,
    /// No principal may make the call once auth lands (it takes from one
    /// private scope and gives to the other), so there is no control call:
    /// the probe names memories the corpus holds, and today it succeeds.
    None,
}

/// A name the corpus does not hold, for a write that would plant a memory.
const PLANTED: &str = "planted-by-tamsin";

/// The writes through which B could change A's scope today: over A's
/// sentinel, under each tool name that writes (`note` routes to `remember`
/// under its own name, so a guard keyed on the others would miss it), and
/// into A's scope where no memory exists yet, by a new name or by moving one
/// of B's own in.
fn write_probes() -> Vec<(&'static str, Value, Control)> {
    vec![
        (
            "edit",
            json!({ "name": A_SENTINEL, "scope": A_SCOPE, "content": "overwritten by B" }),
            Control::Owner,
        ),
        (
            "remember",
            json!({ "name": A_SENTINEL, "scope": A_SCOPE, "content": "collision by B" }),
            Control::Owner,
        ),
        (
            "note",
            json!({ "name": A_SENTINEL, "scope": A_SCOPE, "content": "noted over by B" }),
            Control::Owner,
        ),
        (
            "remember",
            json!({ "name": PLANTED, "scope": A_SCOPE, "content": "planted by B" }),
            Control::Owner,
        ),
        (
            "note",
            json!({ "name": PLANTED, "scope": A_SCOPE, "content": "planted by B" }),
            Control::Owner,
        ),
        (
            "move",
            json!({ "name": A_SENTINEL, "from_scope": A_SCOPE, "to_scope": B_SCOPE }),
            Control::Owner,
        ),
        (
            "move",
            json!({ "name": B_OWN, "from_scope": B_SCOPE, "to_scope": A_SCOPE }),
            Control::None,
        ),
        (
            "forget",
            json!({ "name": A_SENTINEL, "scope": A_SCOPE }),
            Control::Owner,
        ),
    ]
}

/// What A finds in its own scope: the repository does not show a vector
/// written under A's name, and `remember` indexes before it saves, so a
/// guard at the repository would leave one behind with the repository
/// unchanged. A's recall for the sentinel's word carries the distance, and
/// its list the names.
async fn owner_view(a: &McpTestClient) -> Vec<String> {
    let mut seen = Vec::new();
    for (tool, args) in [
        (
            "recall",
            json!({ "query": A_SENTINEL_WORD, "scope": A_SCOPE }),
        ),
        ("list", json!({ "scope": A_SCOPE, "limit": 100 })),
    ] {
        seen.push(format!(
            "{tool} {args} -> {}",
            stable(&call(a, tool, args.clone()).await)
        ));
    }
    seen
}

async fn closed(client: McpTestClient) {
    client
        .close()
        .await
        .unwrap_or_else(|e| panic!("HARNESS BROKEN: a client's session did not close: {e}"));
}

// A07. Red today: B's edit, remember and note (an overwrite, or a new name),
// move (out of A's scope, or into it) and forget each succeed and change the
// repository.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a07_b_writes_to_a_private_scope_change_nothing() {
    let mut wrong = Vec::new();
    for (tool, args, control) in write_probes() {
        // Control, on its own store: A's identical write succeeds.
        // `remember` over an existing name replaces it (an upsert), so for
        // every probe with a control it is a plain success. Each world's
        // clients are closed: this test builds two worlds a probe, and an
        // open session keeps its world's threads.
        if let Control::Owner = control {
            let control_world = world().await;
            let a = control_world.owner().await;
            let reply = call(&a, tool, args.clone()).await;
            assert!(
                matches!(reply, ToolReply::Success(_)),
                "HARNESS BROKEN: A's own {tool} {args} did not succeed: {reply:?}"
            );
            closed(a).await;
        }

        let w = world().await;
        let a = w.owner().await;
        let b = w.other().await;
        let before = (w.truth().await, owner_view(&a).await);
        let reply = call(&b, tool, args.clone()).await;
        let after = (w.truth().await, owner_view(&a).await);
        if before.0 != after.0 {
            wrong.push(format!("{tool} {args} changed the repository ({reply:?})"));
        } else if before.1 != after.1 {
            wrong.push(format!(
                "{tool} {args} left the repository alone and changed what A finds \
                 ({reply:?}):\nbefore: {:?}\nafter:  {:?}",
                before.1, after.1
            ));
        }
        closed(a).await;
        closed(b).await;
    }
    assert!(wrong.is_empty(), "A07:\n{}", wrong.join("\n"));
}

// ---------------------------------------------------------------------------
// A12: what B sees must not change when only A's memory changes
// ---------------------------------------------------------------------------

/// A word nothing in the corpus uses. One of B's keyword-probe memories
/// holds it, and so does A's private note, which raises its document
/// frequency in a keyword index shared across scopes.
const SHARED_WORD: &str = "quillmoth";
/// A word nothing in the corpus uses, held only by B's other probe memory.
const B_ONLY_WORD: &str = "brindlecap";
/// The tag on B's keyword-probe memories, and on nothing else.
const PROBE_TAG: &str = "a12-keyword-probe";
/// A's private note: the change B must not be able to see.
const A_NOTE: &str = "a-new-private-note";
const A_NOTE_CONTENT: &str =
    "Ottoline's own note: a quillmoth again by the bakehouse door this morning.";

/// B's two keyword-probe memories, `(name, content)`, in B's own scope.
///
/// Recall answers carry no BM25 score, only `distance` and `match_type`
/// (`recall_entry_json` in `src/server.rs`), so a shift in keyword
/// statistics shows only as a change of rank. These two are built so that
/// one more document holding [`SHARED_WORD`] reverses their keyword order.
/// Each holds one query word once, and no other document holds either, so
/// the first, at 10 tokens against 15, scores about 6% higher. A second
/// document with [`SHARED_WORD`] cuts that word's IDF by about 12% (document
/// frequency 1 to 2 among about 100 documents), and the second memory then
/// scores about 7% higher. The reversal holds for a corpus average length
/// anywhere from 0.6 to 2 times the real one, about 47 tokens.
const B_PROBE_MEMORIES: [(&str, &str); 2] = [
    (
        "a12-probe-shared-word",
        "Tamsin saw a quillmoth on the vestry window after practice.",
    ),
    (
        "a12-probe-own-word",
        "Tamsin wrote brindlecap on the rota board beside the kettle, then went home to bed.",
    ),
];

/// B's keyword probe. At `limit` 1 recall answers with the keyword path's
/// top hit: fusion scores each path's top hit alike and breaks that tie
/// lexical-first (`src/search/fusion.rs`), so the answer names whichever of
/// [`B_PROBE_MEMORIES`] BM25 ranks first. The tag keeps every other memory
/// out of the candidates, though not out of the statistics.
fn keyword_probe() -> (&'static str, Value) {
    (
        "recall",
        json!({
            "query": format!("{SHARED_WORD} {B_ONLY_WORD}"),
            "scope": B_SCOPE,
            "limit": 1,
            "tags_any": [PROBE_TAG],
        }),
    )
}

/// Calls whose answers to B could carry a trace of A's private changes:
/// counts and pages; the keyword ranking of B's own memories, which BM25
/// statistics shared across scopes would move; recall feedback statistics,
/// which a recall log shared across principals would move; and a move whose
/// answer depends on whether a name exists in A's scope. `recall_stats` runs
/// before the keyword probe, because that probe's own recall is logged for B:
/// read after it, the statistics would carry the BM25 leak's different hit
/// and differ even with only the recall log scoped. The move comes last
/// because, today, it succeeds once the name exists.
///
/// While the name does not exist the move fails, and a failed move marks
/// the keyword index rebuild-required (`src/server.rs`, the move's error
/// arm): recall is semantic-only until a repair converges. That damage is
/// B's own, so [`observe`] waits for a whole index once, after the move
/// and nowhere else. A wait before the probes would also repair an index
/// that A's private change had degraded, and hide from B's keyword probe
/// the very difference A12 exists to show.
fn observation_probes() -> Vec<(&'static str, Value)> {
    vec![
        ("list", json!({ "scope": "all", "limit": 100 })),
        ("list", json!({ "limit": 100 })),
        ("recall_stats", json!({})),
        keyword_probe(),
        (
            "move",
            json!({ "name": A_NOTE, "from_scope": A_SCOPE, "to_scope": B_SCOPE }),
        ),
    ]
}

/// Wait until the keyword index is whole. The failed move that ends
/// [`observation_probes`] leaves it degraded with a repair running in the
/// background; a later keyword probe sent before that repair converges is
/// answered semantic-only, in one world and not the other by timing alone.
/// A repair can also finish with the index flagged again, and then only the
/// next trigger starts another: this asks for one on every turn, as a read
/// would (`spawn_lexical_repair_for_router` is single-flight).
async fn lexical_settled(w: &World, after: &str) {
    for _ in 0..1500 {
        if !w.state.lexical.is_degraded() {
            return;
        }
        spawn_lexical_repair_for_router(&w.state.router, &w.state.lexical);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "HARNESS BROKEN: A12: the keyword index was still degraded 30 s after {after}: the \
         next keyword probe would show that degradation, not the ranking"
    );
}

async fn observe(w: &World, b: &McpTestClient) -> Vec<String> {
    let mut seen = Vec::new();
    for (tool, args) in observation_probes() {
        seen.push(format!(
            "{tool} {args} -> {}",
            stable(&call(b, tool, args.clone()).await)
        ));
    }
    lexical_settled(w, "B's own move probe").await;
    seen
}

fn differences(quiet: &[String], changed: &[String]) -> Vec<String> {
    quiet
        .iter()
        .zip(changed)
        .filter(|(q, c)| q != c)
        .map(|(q, c)| format!("A quiet:   {q}\nA changed: {c}"))
        .collect()
}

fn success_body(reply: &ToolReply, what: &str) -> Value {
    let ToolReply::Success(body) = reply else {
        panic!("control: {what} did not succeed: {reply:?}");
    };
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("control: {what} is not JSON ({e}): {body}"))
}

/// A world in which A has proved the store populated and B holds its
/// keyword-probe memories: `(world, A's client, B's client)`.
async fn a12_world() -> (World, McpTestClient, McpTestClient) {
    let w = world().await;
    let a = w.owner().await;
    let b = w.other().await;
    for (name, content) in B_PROBE_MEMORIES {
        let wrote = call(
            &b,
            "remember",
            json!({ "name": name, "scope": B_SCOPE, "content": content, "tags": [PROBE_TAG] }),
        )
        .await;
        assert!(
            matches!(wrote, ToolReply::Success(_)),
            "HARNESS BROKEN: B's write of {name} to its own scope failed: {wrote:?}"
        );
    }
    (w, a, b)
}

/// A's private change: a note B may not see, a recall that the recall log
/// records, and A's feedback on that recall. Each step is checked as done.
async fn a_private_change(a: &McpTestClient) {
    let wrote = call(
        a,
        "remember",
        json!({ "name": A_NOTE, "scope": A_SCOPE, "content": A_NOTE_CONTENT }),
    )
    .await;
    success_body(&wrote, "A's write");
    let recalled = success_body(
        &call(
            a,
            "recall",
            json!({ "query": SHARED_WORD, "scope": A_SCOPE }),
        )
        .await,
        "A's recall",
    );
    assert!(
        recalled["results"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["name"] == A_NOTE)),
        "control: A's recall did not return {A_NOTE}: {recalled}"
    );
    let recall_id = recalled["recall_id"]
        .as_str()
        .unwrap_or_else(|| panic!("control: A's recall has no recall_id: {recalled}"));
    let marked = success_body(
        &call(
            a,
            "mark_applied",
            json!({ "recall_id": recall_id, "memory": A_NOTE, "verdict": "applied", "confidence": "high" }),
        )
        .await,
        "A's mark_applied",
    );
    assert_eq!(
        marked["rows_affected"], 1,
        "control: the recall log did not record A's feedback: {marked}"
    );
}

// A12. Red today: B's list counts, the keyword ranking of B's own
// memories, the recall statistics and the move answer all differ between
// two identical worlds when A makes a private change in one of them.
#[tokio::test]
#[ignore = "red until inbound auth lands (#379, #383): pali has no client auth"]
async fn a12_b_observations_ignore_a_private_change() {
    // Control: the keyword probe can see a statistics shift at all. B's own
    // untagged note with A's note's text, kept out of the probe's candidates
    // by its tag filter, must reorder the probe as A's note does today. B
    // may see its own memory, so this holds however auth scopes the
    // statistics.
    let (_own_world, _own_a, own_b) = a12_world().await;
    let (tool, args) = keyword_probe();
    let unmoved = stable(&call(&own_b, tool, args.clone()).await);
    let wrote = call(
        &own_b,
        "remember",
        json!({ "name": "a12-b-own-note", "scope": B_SCOPE, "content": A_NOTE_CONTENT }),
    )
    .await;
    success_body(&wrote, "B's write to its own scope");
    let moved = stable(&call(&own_b, tool, args).await);
    assert_ne!(
        unmoved, moved,
        "HARNESS BROKEN: A12: the keyword probe did not move when B added a document holding \
         {SHARED_WORD}, so it cannot show a shift caused by A's either"
    );

    // Two worlds, built and driven alike: B makes the same calls in each,
    // so B's own activity, whose recalls the recall log also counts, cannot
    // be what differs. Only in the second does A make its private change.
    let (quiet_world, _quiet_a, quiet_b) = a12_world().await;
    let (changed_world, changed_a, changed_b) = a12_world().await;

    let quiet = observe(&quiet_world, &quiet_b).await;
    let changed = observe(&changed_world, &changed_b).await;
    let differ = differences(&quiet, &changed);
    assert!(
        differ.is_empty(),
        "HARNESS BROKEN: A12: two identical worlds answer B differently before any change, so \
         a difference after one proves nothing:\n{}",
        differ.join("\n\n")
    );

    a_private_change(&changed_a).await;
    let quiet = observe(&quiet_world, &quiet_b).await;
    let changed = observe(&changed_world, &changed_b).await;
    let differ = differences(&quiet, &changed);
    assert!(
        differ.is_empty(),
        "A12: B's view differs when only A's private scope changed:\n{}",
        differ.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Green today: the public routes and the inventory the sweep relies on
// ---------------------------------------------------------------------------

async fn raw_body(
    w: &World,
    method: &str,
    uri: &str,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body))
        .expect("valid request");
    request.headers_mut().extend(headers);
    let response = w
        .server
        .router()
        .oneshot(request)
        .await
        .expect("router call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

// A01, the named exceptions. Red if a public route starts answering with
// memory names, scopes or content from a populated store.
#[tokio::test]
async fn public_routes_disclose_no_memory() {
    let w = world().await;
    let corpus = common::load_access_corpus();
    for route in PUBLIC_ROUTES {
        let (status, _, body) = raw_body(&w, "GET", route, HeaderMap::new(), String::new()).await;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        assert_names_nothing_of_the_store(&w, &corpus, route, &body);
    }
}

/// Panics if `body` holds a corpus memory's name, a non-global scope, or the
/// path of the repository or of the recall log.
fn assert_names_nothing_of_the_store(w: &World, corpus: &[Memory], route: &str, body: &str) {
    for m in corpus {
        let scope = m.metadata.scope.to_string();
        assert!(
            !body.contains(m.name.as_ref()) && !(scope != "global" && body.contains(&scope)),
            "{route} disclosed {scope}/{}: {body}",
            m.name
        );
    }
    for dir in [&w._repo_dir, &w._recall_log_dir] {
        let path = dir.path().to_string_lossy().into_owned();
        assert!(!body.contains(&path), "{route} disclosed {path}: {body}");
    }
}

// A01, the named exceptions, with subsystems down: the world in which
// `/readyz` says most. A degraded `/readyz` may give its own status, each
// check's `up` or `down`, and a reason from the fixed vocabulary
// (`CheckResult::reason` in `src/health.rs`). Red if a down check starts to
// carry anything else: a new field, or a reason that is not the reporter's
// own static string.
#[tokio::test]
async fn degraded_readyz_discloses_no_memory() {
    let w = world().await;
    let corpus = common::load_access_corpus();
    // Reasons the product itself reports (`src/repo.rs`,
    // `src/embedding/candle.rs`).
    w.state.health.git.report_err("list_memories failed");
    w.state.health.embedding.report_err("embed failed");

    let (status, _, body) = raw_body(&w, "GET", "/readyz", HeaderMap::new(), String::new()).await;
    // Checked first: a world that did not degrade must not pass as the
    // healthy sweep a second time.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "/readyz: {body}");
    assert_names_nothing_of_the_store(&w, &corpus, "/readyz (degraded)", &body);

    let reply: Value = serde_json::from_str(&body).expect("/readyz answers in JSON");
    assert_eq!(
        reply,
        json!({
            "status": "not_ready",
            "checks": {
                "git_repo": { "status": "down", "reason": "list_memories failed" },
                "embedding": { "status": "down", "reason": "embed failed" },
                "vector_index": { "status": "up" },
            },
        }),
        "a degraded /readyz says more, or other, than status, up or down, and a fixed reason"
    );
}

/// What follows each call of `.method(` in `src`, from just inside the
/// parenthesis. Whitespace may sit between the dot, the name and the
/// parenthesis, and a call may span lines, as rustfmt writes a long one.
fn method_call_args<'a>(src: &'a str, method: &str) -> Vec<&'a str> {
    let mut args = Vec::new();
    let mut from = 0;
    while let Some(at) = src[from..].find(method) {
        let start = from + at;
        let end = start + method.len();
        from = end;
        if !src[..start].trim_end().ends_with('.') {
            continue;
        }
        if let Some(inside) = src[end..].trim_start().strip_prefix('(') {
            args.push(inside);
        }
    }
    args
}

// Red if src/main.rs gains or loses a route without this sweep naming it.
// The source is parsed, not run: this PR does not touch src/main.rs to
// expose its router to a test.
#[test]
fn routes_in_main_are_the_ones_swept() {
    let main = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
        .expect("read src/main.rs");
    let found: BTreeSet<&str> = method_call_args(&main, "route")
        .into_iter()
        .map(|inside| {
            inside
                .trim_start()
                .strip_prefix('"')
                .and_then(|path| path.split('"').next())
                .unwrap_or_else(|| {
                    panic!(
                        "a .route( in src/main.rs does not open with a string literal path, so \
                         this sweep cannot name it: {:?}",
                        inside.lines().next().unwrap_or_default()
                    )
                })
        })
        .collect();
    let swept: BTreeSet<&str> = HTTP_ROUTES.iter().copied().collect();
    assert_eq!(found, swept, "router routes vs HTTP_ROUTES");

    // Every other way to add a route. src/main.rs uses none of them today,
    // so a use is a route this sweep does not name.
    for method in [
        "merge",
        "nest",
        "route_service",
        "fallback",
        "fallback_service",
    ] {
        assert!(
            method_call_args(&main, method).is_empty(),
            "src/main.rs now calls .{method}(: name what it serves in HTTP_ROUTES and extend \
             this sweep to read it"
        );
    }
    // The MCP service is the one nested service, at the configured path.
    let nested: Vec<String> = method_call_args(&main, "nest_service")
        .into_iter()
        .map(|inside| {
            inside
                .split(')')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<String>()
        })
        .collect();
    assert_eq!(
        nested,
        ["&mcp_path,service"],
        "the MCP service is no longer the one service nested, where this sweep expects it"
    );
}

// Red if a tool is added or removed without this sweep naming it.
#[tokio::test]
async fn tools_list_is_the_set_swept() {
    let w = world().await;
    let a = w.connect(A_TOKEN).await;
    let reply = a
        .request("tools/list", json!({}))
        .await
        .expect("tools/list");
    let listed: BTreeSet<String> = reply["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_string())
        .collect();
    let swept: BTreeSet<String> = TOOLS.iter().map(|t| t.to_string()).collect();
    assert_eq!(listed, swept);
}

/// The JSON-RPC message in a POST `/mcp` answer, whether the transport sent
/// it as JSON or as one event of a stream.
fn rpc_message(body: &str) -> Value {
    body.lines()
        .map(|line| line.strip_prefix("data:").unwrap_or(line).trim())
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| panic!("HARNESS BROKEN: no JSON-RPC message in: {body}"))
}

// A06, completion. pali offers tools and nothing else, so there is no prompt
// or resource for a completion to be asked about, and a probe of one would
// pass whatever the handler did. This pins the absence instead: no such
// capability is advertised, and each method gives an error or an empty list.
// Red when pali gains completion, a prompt or a resource; whoever adds one
// replaces that arm with a probe of a ref that then exists, asked by B on
// this populated store.
#[tokio::test]
async fn methods_outside_tools_offer_no_memory() {
    let w = world().await;
    let _a = w.owner().await;
    let b = w.other().await;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "pali-isolation", "version": "0" },
        },
    });
    let (status, _, body) = raw_body(&w, "POST", "/mcp", bearer(B_TOKEN), init.to_string()).await;
    assert_eq!(status, StatusCode::OK, "initialize: {body}");
    let capabilities = &rpc_message(&body)["result"]["capabilities"];
    // The control: the capabilities were read from where they are.
    assert!(
        capabilities.get("tools").is_some(),
        "HARNESS BROKEN: initialize advertised no tools: {body}"
    );
    for surface in ["completions", "prompts", "resources"] {
        assert!(
            capabilities.get(surface).is_none(),
            "pali now advertises {surface}: probe it as B before this test may pass"
        );
    }

    let completion = json!({
        "ref": { "type": "ref/prompt", "name": "recall" },
        "argument": { "name": "scope", "value": "otto" },
    });
    for (method, params, listed) in [
        ("completion/complete", completion, "/completion/values"),
        ("prompts/list", json!({}), "/prompts"),
        ("resources/list", json!({}), "/resources"),
        ("resources/templates/list", json!({}), "/resourceTemplates"),
    ] {
        let reply = b.request(method, params).await.expect("an answer");
        let body = reply.to_string();
        // A refusal offers nothing. A result must hold its list, empty: a
        // result of another shape is not read as empty.
        let offers_nothing = match reply.get("result") {
            None => reply.get("error").is_some(),
            Some(result) => result
                .pointer(listed)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        };
        assert!(offers_nothing, "{method} offered something: {body}");
        assert!(
            !body.contains(A_SCOPE) && !body.contains(A_SENTINEL) && !body.contains("sentinel"),
            "{method} offered A's memory: {body}"
        );
    }
}
