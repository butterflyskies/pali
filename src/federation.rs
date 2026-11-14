//! Custody-preserving reads across independently authorized Pali stores.
//!
//! Federation is intentionally read-only in this slice. Sibling topology is
//! configured by an administrator, while authority is supplied by the caller
//! on each request and forwarded opaquely to each sibling.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    service::ServiceError,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use serde::Serialize;

use crate::config::Config;

/// One store's contribution to a straddled read.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StoreReadResult {
    /// Stable machine-readable outcome.
    pub status: StoreReadStatus,
    /// Full legacy `read` response when the memory was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<serde_json::Value>,
}

/// Outcomes retain the distinction between absence and inability to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoreReadStatus {
    /// The store returned the memory.
    Found,
    /// The store authoritatively reported no memory by that name.
    NotFound,
    /// The store could not be reached or returned an unusable response.
    Unreachable,
    /// No caller delegation was available, so the sibling was not contacted.
    IdentityUnavailable,
}

impl StoreReadResult {
    pub(crate) fn found(memory: serde_json::Value) -> Self {
        Self {
            status: StoreReadStatus::Found,
            memory: Some(memory),
        }
    }

    pub(crate) fn status(status: StoreReadStatus) -> Self {
        Self {
            status,
            memory: None,
        }
    }

    pub(crate) fn is_degraded(&self) -> bool {
        matches!(
            self.status,
            StoreReadStatus::Unreachable | StoreReadStatus::IdentityUnavailable
        )
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Trusted sibling topology and its bounded request deadline.
#[derive(Clone)]
pub(crate) struct Federation {
    store_id: String,
    siblings: BTreeMap<String, String>,
    timeout: Duration,
    client: Arc<dyn SiblingReadClient>,
}

impl Default for Federation {
    fn default() -> Self {
        Self {
            store_id: "local".to_owned(),
            siblings: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            client: Arc::new(RmcpSiblingReadClient {
                timeout: DEFAULT_TIMEOUT,
            }),
        }
    }
}

impl Federation {
    /// Build federation topology from validated configuration.
    pub(crate) fn from_config(config: &Config) -> Result<Self, crate::error::MemoryError> {
        config.validate_siblings()?;
        let timeout = Duration::from_millis(config.straddle_timeout_ms);
        Ok(Self {
            store_id: config.store_id.clone(),
            siblings: config.siblings.clone(),
            timeout,
            client: Arc::new(RmcpSiblingReadClient { timeout }),
        })
    }

    pub(crate) fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Read every sibling concurrently, bounded by one shared deadline.
    ///
    /// `bearer` is a caller-supplied opaque delegation without the `Bearer`
    /// prefix. It is moved into request tasks, never persisted or logged.
    pub(crate) async fn read_siblings(
        &self,
        name: &str,
        scope: &str,
        bearer: Option<String>,
    ) -> BTreeMap<String, StoreReadResult> {
        let Some(bearer) = bearer else {
            return self
                .siblings
                .keys()
                .cloned()
                .map(|id| {
                    (
                        id,
                        StoreReadResult::status(StoreReadStatus::IdentityUnavailable),
                    )
                })
                .collect();
        };

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut tasks = Vec::with_capacity(self.siblings.len());
        for (store_id, endpoint) in &self.siblings {
            let store_id = store_id.clone();
            let endpoint = endpoint.clone();
            let name = name.to_owned();
            let scope = scope.to_owned();
            let bearer = bearer.clone();
            let client = Arc::clone(&self.client);
            let task =
                tokio::spawn(async move { client.read(&endpoint, bearer, &name, &scope).await });
            tasks.push((store_id, task));
        }
        // Drop the final copy before awaiting responses. No credential is
        // retained in the federation object or result envelope.
        drop(bearer);

        let mut results = BTreeMap::new();
        for (store_id, mut task) in tasks {
            let result = match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(result)) => result,
                Ok(Err(_join_error)) => StoreReadResult::status(StoreReadStatus::Unreachable),
                Err(_elapsed) => {
                    task.abort();
                    StoreReadResult::status(StoreReadStatus::Unreachable)
                }
            };
            results.insert(store_id, result);
        }
        results
    }
}

#[async_trait::async_trait]
trait SiblingReadClient: Send + Sync {
    async fn read(
        &self,
        endpoint: &str,
        bearer: String,
        name: &str,
        scope: &str,
    ) -> StoreReadResult;
}

/// Reads a sibling over rmcp's Streamable HTTP client.
///
/// `timeout` is set on the HTTP client itself, not only on the caller's wait.
/// rmcp runs the transport in a task of its own and reads a JSON response
/// body there, so aborting the caller does not stop that read; without a
/// client timeout an endless body is buffered until the process runs out of
/// memory (RUSTSEC-2026-0462).
struct RmcpSiblingReadClient {
    timeout: Duration,
}

#[async_trait::async_trait]
impl SiblingReadClient for RmcpSiblingReadClient {
    async fn read(
        &self,
        endpoint: &str,
        bearer: String,
        name: &str,
        scope: &str,
    ) -> StoreReadResult {
        read_one_sibling(endpoint, bearer, name, scope, self.timeout).await
    }
}

async fn read_one_sibling(
    endpoint: &str,
    bearer: String,
    name: &str,
    scope: &str,
    timeout: Duration,
) -> StoreReadResult {
    // rmcp's own default client, plus a total timeout that covers the body.
    let http = match rmcp_reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(timeout)
        .build()
    {
        Ok(http) => http,
        Err(_error) => return StoreReadResult::status(StoreReadStatus::Unreachable),
    };
    let transport = StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned()).auth_header(bearer),
    );
    let client = match ClientInfo::default().serve(transport).await {
        Ok(client) => client,
        Err(_error) => return StoreReadResult::status(StoreReadStatus::Unreachable),
    };
    let arguments = match serde_json::json!({ "name": name, "scope": scope }) {
        serde_json::Value::Object(arguments) => arguments,
        _ => unreachable!("read arguments are always a JSON object"),
    };
    let result = client
        .call_tool(CallToolRequestParams::new("read").with_arguments(arguments))
        .await;
    let outcome = match result {
        Ok(result) if result.is_error != Some(true) => {
            let structured_content = result.structured_content;
            let content = result.content;
            structured_content
                .or_else(|| {
                    content
                        .first()
                        .and_then(|content| content.raw.as_text())
                        .and_then(|text| serde_json::from_str(&text.text).ok())
                })
                .map(StoreReadResult::found)
                .unwrap_or_else(|| StoreReadResult::status(StoreReadStatus::Unreachable))
        }
        Err(ServiceError::McpError(error)) if error.message.starts_with("memory not found:") => {
            StoreReadResult::status(StoreReadStatus::NotFound)
        }
        Ok(_) | Err(_) => StoreReadResult::status(StoreReadStatus::Unreachable),
    };
    let _ = client.cancel().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingClient {
        calls: Mutex<Vec<(String, String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl SiblingReadClient for RecordingClient {
        async fn read(
            &self,
            endpoint: &str,
            bearer: String,
            name: &str,
            scope: &str,
        ) -> StoreReadResult {
            self.calls.lock().unwrap().push((
                endpoint.to_owned(),
                bearer,
                name.to_owned(),
                scope.to_owned(),
            ));
            StoreReadResult::status(StoreReadStatus::NotFound)
        }
    }

    struct SlowClient;

    #[async_trait::async_trait]
    impl SiblingReadClient for SlowClient {
        async fn read(
            &self,
            _endpoint: &str,
            _bearer: String,
            _name: &str,
            _scope: &str,
        ) -> StoreReadResult {
            tokio::time::sleep(Duration::from_secs(60)).await;
            StoreReadResult::status(StoreReadStatus::NotFound)
        }
    }

    fn test_federation(client: Arc<dyn SiblingReadClient>) -> Federation {
        Federation {
            store_id: "personal".into(),
            siblings: BTreeMap::from([("fcc".into(), "https://fcc.example/mcp".into())]),
            timeout: Duration::from_secs(30),
            client,
        }
    }

    #[tokio::test]
    async fn missing_identity_never_attempts_sibling_io() {
        let client = Arc::new(RecordingClient::default());
        let federation = test_federation(client.clone());

        let result = federation
            .read_siblings("person-cammy", "global", None)
            .await;

        assert_eq!(result["fcc"].status, StoreReadStatus::IdentityUnavailable);
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn caller_delegation_and_read_target_are_forwarded_without_straddle() {
        let client = Arc::new(RecordingClient::default());
        let federation = test_federation(client.clone());

        let result = federation
            .read_siblings("person-cammy", "global", Some("caller-delegation".into()))
            .await;

        assert_eq!(result["fcc"].status, StoreReadStatus::NotFound);
        assert_eq!(
            client.calls.lock().unwrap().as_slice(),
            &[(
                "https://fcc.example/mcp".into(),
                "caller-delegation".into(),
                "person-cammy".into(),
                "global".into(),
            )]
        );
    }

    #[tokio::test]
    async fn shared_deadline_marks_slow_sibling_unreachable() {
        let mut federation = test_federation(Arc::new(SlowClient));
        federation.timeout = Duration::from_millis(1);

        let started = tokio::time::Instant::now();
        let result = federation
            .read_siblings("person-cammy", "global", Some("delegation".into()))
            .await;

        assert_eq!(result["fcc"].status, StoreReadStatus::Unreachable);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Serve one response of `content_type` whose chunked body starts with
    /// `opening` and then never ends, read it as a sibling with a 200 ms
    /// deadline, and return how long the server could keep writing. That the
    /// caller got `Unreachable` is not the point: rmcp's transport task
    /// outlives the caller, so the server is the witness of whether anything
    /// is still reading.
    async fn endless_body_closed_after(content_type: &str, opening: &str) -> Duration {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{opening}\r\n",
            opening.len()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(head.as_bytes()).await.unwrap();
            // Spaces are JSON whitespace, and inside an SSE `data:` line they
            // are more of a line that never reaches its newline.
            let chunk = format!("{:x}\r\n{}\r\n", 1024, " ".repeat(1024));
            let started = tokio::time::Instant::now();
            while started.elapsed() < Duration::from_secs(10) {
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    return Some(started.elapsed());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            None
        });

        let timeout = Duration::from_millis(200);
        let federation = Federation {
            store_id: "personal".into(),
            siblings: BTreeMap::from([("fcc".into(), endpoint)]),
            timeout,
            client: Arc::new(RmcpSiblingReadClient { timeout }),
        };
        let result = federation
            .read_siblings("person-cammy", "global", Some("delegation".into()))
            .await;
        assert_eq!(result["fcc"].status, StoreReadStatus::Unreachable);

        server
            .await
            .unwrap()
            .expect("the client was still reading 10 s after a 200 ms deadline")
    }

    #[tokio::test]
    async fn endless_json_body_loses_its_connection_at_the_deadline() {
        let closed_after = endless_body_closed_after("application/json", " ").await;
        assert!(closed_after < Duration::from_secs(5), "{closed_after:?}");
    }

    /// The same attack by the other door: rmcp 1.8's SSE reader (sse-stream
    /// 0.2) has no cap on a line that never ends.
    #[tokio::test]
    async fn endless_sse_line_loses_its_connection_at_the_deadline() {
        let closed_after = endless_body_closed_after("text/event-stream", "data: ").await;
        assert!(closed_after < Duration::from_secs(5), "{closed_after:?}");
    }
}
