//! Process-level OTLP startup regressions: credential-safe exporter
//! construction errors.
//!
//! Repro shape (review finding): a live TCP listener behind an `https://`
//! endpoint whose query string carries a secret. The startup TCP probe
//! passes (the port accepts connections), but exporter construction then
//! fails — this build compiles the tonic exporter without a TLS feature, so
//! an HTTPS endpoint is rejected with an SDK error that embeds the full URL,
//! secret included. The construction-failure message must contain only the
//! sanitized `scheme://host:port` (plus error kind and env source), never
//! the query string or the secret.
//!
//! These are subprocess tests because the behavior under test is process
//! startup: what reaches stderr and whether the process exits or binds.
#![cfg(feature = "otlp")]

#[path = "common/subprocess.rs"]
mod subprocess_support;

use std::time::Duration;

/// A secret that must never appear on stderr.
const SECRET: &str = "super-secret";

/// Bind a live listener and return it with an HTTPS OTLP endpoint pointing
/// at it, carrying [`SECRET`] in the query string. The listener accepts TCP
/// connections at the OS level without an accept loop, which is all the
/// startup probe needs.
fn live_https_endpoint_with_secret() -> (std::net::TcpListener, u16, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let endpoint = format!("https://127.0.0.1:{port}/v1/traces?api_key={SECRET}");
    (listener, port, endpoint)
}

/// Regression: `--otlp-required` exporter construction failure must print
/// the sanitized `scheme://host:port` + env source and exit non-zero —
/// previously the raw SDK error (which embeds the full URL, secret
/// included) was printed verbatim.
#[tokio::test]
async fn otlp_required_construction_failure_sanitizes_stderr_and_exits_nonzero() {
    let (_listener, port, endpoint) = live_https_endpoint_with_secret();
    let tmp = tempfile::tempdir().expect("tempdir");

    let output = subprocess_support::pali_command()
        .args([
            "serve",
            "--repo-path",
            tmp.path().to_str().expect("utf8 temp path"),
            "--otlp-required",
        ])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .output()
        .await
        .expect("failed to run pali serve");

    assert!(
        !output.status.success(),
        "serve --otlp-required must exit non-zero when exporter construction fails, got: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("https://127.0.0.1:{port}")),
        "stderr should name the sanitized endpoint: {stderr}"
    );
    assert!(
        stderr.contains("OTEL_EXPORTER_OTLP_ENDPOINT"),
        "stderr should name the supplying env var: {stderr}"
    );
    assert!(
        !stderr.contains(SECRET) && !stderr.contains("api_key"),
        "credential leaked into stderr: {stderr}"
    );
}

/// Regression: `--otlp-optional` exporter construction failure must warn
/// with the sanitized endpoint (never the secret) and then continue to bind
/// and serve.
#[tokio::test]
async fn otlp_optional_construction_failure_sanitizes_warning_and_binds() {
    let (_listener, otlp_port, endpoint) = live_https_endpoint_with_secret();
    let tmp = tempfile::tempdir().expect("tempdir");
    let bind_port = portpicker::pick_unused_port().expect("no free port");
    let bind = format!("127.0.0.1:{bind_port}");

    // Stderr goes to a file, not a pipe: the child must never block on a
    // full pipe buffer while we poll /healthz, and the warning is written
    // during tracing init, long before readiness.
    let stderr_path = tmp.path().join("serve-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr capture file");

    let mut child = subprocess_support::pali_command()
        .args([
            "serve",
            "--repo-path",
            tmp.path().to_str().expect("utf8 temp path"),
            "--otlp-optional",
        ])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env("PALI_BIND", &bind)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("failed to start pali serve");

    // Same readiness budget as the PALI_BIND test: the embedding model
    // loads synchronously before the listener binds.
    let client = reqwest::Client::new();
    let healthz_url = format!("http://{bind}/healthz");
    let mut ready = false;
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
    let stderr = std::fs::read_to_string(&stderr_path).expect("read captured stderr");

    assert!(
        ready,
        "serve --otlp-optional must continue to bind after exporter construction \
         failure; stderr: {stderr}"
    );
    assert!(
        stderr.contains("continuing with fmt-only tracing"),
        "stderr should carry the fallback warning: {stderr}"
    );
    assert!(
        stderr.contains(&format!("https://127.0.0.1:{otlp_port}")),
        "warning should name the sanitized endpoint: {stderr}"
    );
    assert!(
        !stderr.contains(SECRET) && !stderr.contains("api_key"),
        "credential leaked into stderr: {stderr}"
    );
}
