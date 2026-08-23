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
            timeout: Duration::from_millis(2_000),
            client: Arc::new(RmcpSiblingReadClient),
        }
    }
}

impl Federation {
    /// Build federation topology from validated configuration.
    pub(crate) fn from_config(config: &Config) -> Result<Self, crate::error::MemoryError> {
        config.validate_siblings()?;
        Ok(Self {
            store_id: config.store_id.clone(),
            siblings: config.siblings.clone(),
            timeout: Duration::from_millis(config.straddle_timeout_ms),
            client: Arc::new(RmcpSiblingReadClient),
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

struct RmcpSiblingReadClient;

#[async_trait::async_trait]
impl SiblingReadClient for RmcpSiblingReadClient {
    async fn read(
        &self,
        endpoint: &str,
        bearer: String,
        name: &str,
        scope: &str,
    ) -> StoreReadResult {
        read_one_sibling(endpoint, bearer, name, scope).await
    }
}

async fn read_one_sibling(
    endpoint: &str,
    bearer: String,
    name: &str,
    scope: &str,
) -> StoreReadResult {
    let transport = StreamableHttpClientTransport::from_config(
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
}
