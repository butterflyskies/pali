//! Integration tests for `auth login`, `auth status`, and `MEMORY_MCP_BIND`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pali::auth::{device_flow_login, DeviceFlowProvider, StoreBackend};
use pali::error::MemoryError;

// ---------------------------------------------------------------------------
// Mock DeviceFlowProvider
// ---------------------------------------------------------------------------

struct MockProvider {
    device_code_url: String,
    access_token_url: String,
}

impl DeviceFlowProvider for MockProvider {
    fn client_id(&self) -> &str {
        "mock-client-id-1234"
    }

    fn device_code_url(&self) -> &str {
        &self.device_code_url
    }

    fn access_token_url(&self) -> &str {
        &self.access_token_url
    }

    fn scopes(&self) -> &[&str] {
        &["repo"]
    }

    fn validate(&self) -> Result<(), MemoryError> {
        for (url, name) in [
            (&self.device_code_url, "device_code_url"),
            (&self.access_token_url, "access_token_url"),
        ] {
            let parsed = reqwest::Url::parse(url)
                .map_err(|e| MemoryError::OAuth(format!("invalid {name} URL: {e}")))?;
            match parsed.scheme() {
                "https" => {}
                "http"
                    if matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => {}
                _ => {
                    return Err(MemoryError::OAuth(format!(
                        "{name} must use HTTPS (got {url})"
                    )));
                }
            }
        }
        if self.client_id().len() < 4 || self.client_id().len() > 64 {
            return Err(MemoryError::OAuth("client ID length out of range".into()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Mock server helper
// ---------------------------------------------------------------------------

/// RAII guard that aborts the mock server task on drop.
struct MockServerGuard(tokio::task::JoinHandle<()>);

impl Drop for MockServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn a mock OAuth server and return the base URL, the call counter for
/// the `/oauth/token` endpoint, and an RAII guard that aborts the server on drop.
///
/// `token_responses` is a list of JSON bodies returned in order on each
/// `POST /oauth/token` call. The last entry is repeated for any subsequent calls.
async fn spawn_mock_server(
    token_responses: Vec<serde_json::Value>,
) -> (String, Arc<AtomicUsize>, MockServerGuard) {
    use axum::routing::post;
    use axum::Router;

    let call_count = Arc::new(AtomicUsize::new(0));
    let responses = Arc::new(token_responses);

    let call_count_clone = Arc::clone(&call_count);
    let responses_clone = Arc::clone(&responses);

    let router = Router::new()
        .route(
            "/device/code",
            post(|| async {
                axum::Json(serde_json::json!({
                    "device_code": "dc_test",
                    "user_code": "USER-1234",
                    "verification_uri": "http://example.com",
                    "expires_in": 300,
                    "interval": 1
                }))
            }),
        )
        .route(
            "/oauth/token",
            post(
                move |axum::extract::Form(fields): axum::extract::Form<HashMap<String, String>>| {
                    let responses = Arc::clone(&responses_clone);
                    let call_count = Arc::clone(&call_count_clone);
                    async move {
                        assert!(
                            fields.contains_key("client_id"),
                            "missing client_id in token request"
                        );
                        assert!(
                            fields.contains_key("device_code"),
                            "missing device_code in token request"
                        );
                        assert!(
                            fields.contains_key("grant_type"),
                            "missing grant_type in token request"
                        );
                        let idx = call_count.fetch_add(1, Ordering::SeqCst);
                        let last = responses.len().saturating_sub(1);
                        let resp = &responses[idx.min(last)];
                        axum::Json(resp.clone())
                    }
                },
            ),
        );

    let port = portpicker::pick_unused_port().expect("no free port");
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind mock server");

    let base_url = format!("http://127.0.0.1:{port}");

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // The listener is already bound; a brief yield lets the server task start
    // accepting connections.
    tokio::task::yield_now().await;

    (base_url, call_count, MockServerGuard(handle))
}

// ---------------------------------------------------------------------------
// Tests 1 & 2: auth status (subprocess)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_status_no_token_prints_not_configured() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_memory-mcp"))
        .args(["auth", "status"])
        .env_remove("MEMORY_MCP_GITHUB_TOKEN")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("DISPLAY")
        .env("XDG_RUNTIME_DIR", tmp.path())
        .env("HOME", tmp.path())
        .output()
        .await
        .expect("failed to run memory-mcp auth status");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No token configured"),
        "expected 'No token configured' in stdout, got: {stdout}"
    );
}

#[tokio::test]
async fn auth_status_with_env_token_prints_source_and_preview() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let token = "ghp_test1234abcdefgh";

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_memory-mcp"))
        .args(["auth", "status"])
        .env("MEMORY_MCP_GITHUB_TOKEN", token)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("HOME", tmp.path())
        .output()
        .await
        .expect("failed to run memory-mcp auth status");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("environment variable"),
        "expected 'environment variable' in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("efgh"),
        "expected last-4-chars preview 'efgh' in stdout, got: {stdout}"
    );
    assert!(
        !stdout.contains(token),
        "stdout must not contain the full token"
    );
}

// ---------------------------------------------------------------------------
// Tests 3–5: device flow (in-process with mock server, paused tokio time)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn auth_login_device_flow_with_mock_server() {
    let token_responses = vec![
        serde_json::json!({"error": "authorization_pending"}),
        serde_json::json!({"access_token": "ghp_mock_token_xyz", "token_type": "bearer"}),
    ];

    let (base_url, _, _guard) = spawn_mock_server(token_responses).await;

    let provider = MockProvider {
        device_code_url: format!("{base_url}/device/code"),
        access_token_url: format!("{base_url}/oauth/token"),
    };

    let result = device_flow_login(
        &provider,
        Some(StoreBackend::Stdout),
        #[cfg(feature = "k8s")]
        None,
    )
    .await;

    assert!(result.is_ok(), "expected Ok(()), got: {result:?}");
}

#[tokio::test(start_paused = true)]
async fn auth_login_device_flow_access_denied() {
    let token_responses = vec![serde_json::json!({"error": "access_denied"})];

    let (base_url, _, _guard) = spawn_mock_server(token_responses).await;

    let provider = MockProvider {
        device_code_url: format!("{base_url}/device/code"),
        access_token_url: format!("{base_url}/oauth/token"),
    };

    let result = device_flow_login(
        &provider,
        Some(StoreBackend::Stdout),
        #[cfg(feature = "k8s")]
        None,
    )
    .await;

    let err = result.expect_err("expected Err for access_denied");
    let msg = err.to_string();
    assert!(
        msg.contains("denied"),
        "error message should contain 'denied', got: {msg}"
    );
}

#[tokio::test(start_paused = true)]
async fn auth_login_device_flow_slow_down_backoff() {
    let token_responses = vec![
        serde_json::json!({"error": "slow_down"}),
        serde_json::json!({"error": "slow_down"}),
        serde_json::json!({"access_token": "ghp_ok", "token_type": "bearer"}),
    ];

    let (base_url, call_count, _guard) = spawn_mock_server(token_responses).await;

    let provider = MockProvider {
        device_code_url: format!("{base_url}/device/code"),
        access_token_url: format!("{base_url}/oauth/token"),
    };

    let before = tokio::time::Instant::now();
    let result = device_flow_login(
        &provider,
        Some(StoreBackend::Stdout),
        #[cfg(feature = "k8s")]
        None,
    )
    .await;
    let elapsed = before.elapsed();

    assert!(result.is_ok(), "expected Ok(()), got: {result:?}");
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        3,
        "mock should have received exactly 3 token polls"
    );
    // Verify backoff: interval starts at 1, becomes 6 after first slow_down,
    // then 11 after second. Total sleep = 1 + 6 + 11 = 18s virtual time.
    assert!(
        elapsed >= Duration::from_secs(17),
        "expected at least 17s virtual time for backoff (1+6+11), got {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests 6 & 7: MEMORY_MCP_BIND env var and CLI override
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pali_bind_env_var_sets_listen_address() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().to_str().expect("non-utf8 temp path");
    let port = portpicker::pick_unused_port().expect("no free port");
    let bind = format!("127.0.0.1:{port}");

    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_memory-mcp"));
    cmd.args(["serve", "--repo-path", repo_path])
        .env("MEMORY_MCP_BIND", &bind)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().expect("failed to start memory-mcp");

    let client = reqwest::Client::new();
    let healthz_url = format!("http://{bind}/healthz");
    let mut ready = false;
    // The server loads the embedding model synchronously before binding the
    // HTTP listener, so /healthz is unreachable until model load completes. On
    // a cold HuggingFace cache that includes downloading ~130 MB, which can take
    // tens of seconds on a loaded CI runner. Budget 60s to match the embedding
    // integration tests' TEST_TIMEOUT.
    for _ in 0..600 {
        if let Ok(resp) = client.get(&healthz_url).send().await {
            if resp.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    child.kill().await.ok();
    assert!(
        ready,
        "server on MEMORY_MCP_BIND={bind} did not become ready"
    );
}
