use thiserror::Error;

/// Errors produced by the memory engine.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// An operation on the git-backed store failed.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    /// The embedding backend failed to produce vectors.
    #[error("embedding error: {0}")]
    Embedding(String),

    /// A queued embedding request did not start within its deadline.
    #[error(
        "embedding error: timed out waiting {timeout_secs:.1}s for the embedding worker — the worker will recover automatically"
    )]
    EmbeddingQueueTimeout {
        /// Configured request deadline in seconds.
        timeout_secs: f64,
    },

    /// Active embedding inference did not complete within its deadline.
    #[error(
        "embedding error: active inference timed out after {timeout_secs:.1}s — the worker will recover automatically"
    )]
    EmbeddingInferenceTimeout {
        /// Configured request deadline in seconds.
        timeout_secs: f64,
    },

    /// The embedding worker's bounded queue had no capacity for this request.
    #[error("embedding error: embedding worker is busy — try again")]
    EmbeddingWorkerBusy,

    /// The embedding worker cannot accept or answer requests.
    #[error("embedding error: embedding worker unavailable — {reason}")]
    EmbeddingWorkerUnavailable {
        /// Why the worker is unavailable.
        reason: &'static str,
    },

    /// The vector index could not complete the requested operation.
    #[error("index error: {0}")]
    Index(String),

    /// A filesystem I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The requested memory does not exist.
    #[error("memory not found: {name}")]
    NotFound {
        /// Name of the missing memory.
        name: String,
    },

    /// The caller provided invalid parameters.
    #[error("invalid input: {reason}")]
    InvalidInput {
        /// Why the input was rejected.
        reason: String,
    },

    /// Authentication failed (e.g. bad credentials).
    #[error("auth error: {0}")]
    Auth(String),

    /// An OAuth flow error occurred.
    #[error("oauth error: {0}")]
    OAuth(String),

    /// The credential store could not read or write a token.
    #[error("token storage error: {0}")]
    TokenStorage(String),

    /// YAML serialisation or deserialisation failed.
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    /// The remote server rejected one or more ref updates during push.
    #[error("push rejected: {0}")]
    PushRejected(String),

    /// A background task failed to join.
    #[error("task join error: {0}")]
    Join(String),

    /// Catch-all for unexpected internal failures.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<MemoryError> for rmcp::model::ErrorData {
    fn from(err: MemoryError) -> Self {
        let code = match &err {
            MemoryError::NotFound { .. } | MemoryError::InvalidInput { .. } => {
                rmcp::model::ErrorCode::INVALID_PARAMS
            }
            // PushRejected is a server-side policy decision (e.g. branch
            // protection), not an internal fault. No standard JSON-RPC code
            // fits precisely; INTERNAL_ERROR is the least-bad option until
            // MCP defines application-level error codes.
            _ => rmcp::model::ErrorCode::INTERNAL_ERROR,
        };
        rmcp::model::ErrorData {
            code,
            message: std::borrow::Cow::Owned(err.to_string()),
            data: None,
        }
    }
}
