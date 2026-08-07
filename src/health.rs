//! Health reporting infrastructure and HTTP handlers.
//!
//! Subsystems report their own operational state via [`crate::health::SubsystemReporter`].
//! The `/readyz` handler reads the latest state — no active probing.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::response::IntoResponse;

// ---------------------------------------------------------------------------
// SubsystemStatus
// ---------------------------------------------------------------------------

/// Per-subsystem health snapshot. Immutable once created.
pub struct SubsystemStatus {
    /// Whether the subsystem is currently healthy.
    pub healthy: bool,
    /// Human-readable reason for an unhealthy state. `None` when healthy.
    pub reason: Option<&'static str>,
    /// Timestamp of the most recent successful operation.
    /// Used for staleness detection — if healthy but last success is old, may be stale.
    pub last_success: Option<Instant>,
    /// Timestamp of the most recent failed operation.
    pub last_failure: Option<Instant>,
}

impl SubsystemStatus {
    fn initial() -> Self {
        Self {
            healthy: false,
            reason: Some("not yet checked"),
            last_success: None,
            last_failure: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SubsystemReporter
// ---------------------------------------------------------------------------

/// Lightweight handle for a subsystem to report its health.
///
/// Clone is cheap (Arc clone). Each clone shares the same underlying state.
#[derive(Clone)]
pub struct SubsystemReporter {
    state: Arc<ArcSwap<SubsystemStatus>>,
}

impl SubsystemReporter {
    /// Create a new reporter. Initial state is "not yet checked" (unhealthy until first success).
    pub fn new() -> Self {
        Self {
            state: Arc::new(ArcSwap::new(Arc::new(SubsystemStatus::initial()))),
        }
    }

    /// Report a successful operation.
    pub fn report_ok(&self) {
        self.state.rcu(|current| {
            Arc::new(SubsystemStatus {
                healthy: true,
                reason: None,
                last_success: Some(Instant::now()),
                last_failure: current.last_failure,
            })
        });
    }

    /// Report a failed operation with a static reason string.
    pub fn report_err(&self, reason: &'static str) {
        self.state.rcu(|current| {
            Arc::new(SubsystemStatus {
                healthy: false,
                reason: Some(reason),
                last_success: current.last_success,
                last_failure: Some(Instant::now()),
            })
        });
    }

    /// Load the current status snapshot.
    pub fn load(&self) -> arc_swap::Guard<Arc<SubsystemStatus>> {
        self.state.load()
    }
}

impl Default for SubsystemReporter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HealthRegistry
// ---------------------------------------------------------------------------

/// Central registry holding reporters for all subsystems.
///
/// Lives on `AppState`. The `/readyz` handler reads from here — no active probing.
pub struct HealthRegistry {
    /// Reporter for the git-backed memory repository.
    pub git: SubsystemReporter,
    /// Reporter for the embedding engine.
    pub embedding: SubsystemReporter,
    /// Reporter for the vector index.
    pub vector_index: SubsystemReporter,
    /// Reporter for remote sync (push/pull) operations.
    pub sync: SubsystemReporter,
    /// Whether sync failures affect readiness (`/readyz` returns 503 on sync failure).
    pub require_sync: bool,
    /// Duration after which a healthy subsystem with no recent successes is
    /// considered stale. `None` disables staleness detection.
    pub stale_threshold: Option<Duration>,
    was_ready: AtomicBool,
}

impl HealthRegistry {
    /// Create a new registry with all subsystems in "not yet checked" state.
    ///
    /// `require_sync` controls whether sync failures affect readiness.
    /// `stale_threshold` sets the staleness window (`None` to disable).
    pub fn new() -> Self {
        Self::with_config(false, None)
    }

    /// Create a registry with explicit sync-gating and staleness configuration.
    pub fn with_config(require_sync: bool, stale_threshold: Option<Duration>) -> Self {
        Self {
            git: SubsystemReporter::new(),
            embedding: SubsystemReporter::new(),
            vector_index: SubsystemReporter::new(),
            sync: SubsystemReporter::new(),
            require_sync,
            stale_threshold,
            was_ready: AtomicBool::new(false),
        }
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Response types for JSON serialisation
// ---------------------------------------------------------------------------

/// Top-level readiness response returned by `/readyz`.
#[derive(serde::Serialize)]
pub struct ReadyzResponse {
    /// `"ready"` or `"not_ready"`.
    pub status: &'static str,
    /// Per-subsystem check results.
    pub checks: ReadyzChecks,
}

/// Individual subsystem check results.
#[derive(serde::Serialize)]
pub struct ReadyzChecks {
    /// Git repository lock status.
    pub git_repo: CheckResult,
    /// Embedding model status.
    pub embedding: CheckResult,
    /// Vector index status.
    pub vector_index: CheckResult,
    /// Remote sync status. Present only when `--require-remote-sync` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<CheckResult>,
}

/// Result of a single subsystem health check.
#[derive(serde::Serialize)]
pub struct CheckResult {
    /// `"up"` or `"down"`.
    pub status: &'static str,
    /// Present only when `status` is `"down"` — fixed vocabulary, no dynamic content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl From<&SubsystemStatus> for CheckResult {
    fn from(s: &SubsystemStatus) -> Self {
        if s.healthy {
            Self {
                status: "up",
                reason: None,
            }
        } else {
            Self {
                status: "down",
                reason: s.reason,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Staleness helpers
// ---------------------------------------------------------------------------

/// Derive a `CheckResult` from a status snapshot, applying staleness detection
/// when `threshold` is `Some`.
///
/// A subsystem that reports `healthy = true` but whose `last_success` is older
/// than `threshold` is considered stale and returned as `"down"` with reason
/// `"stale"`.
fn check_with_staleness(status: &SubsystemStatus, threshold: Option<Duration>) -> CheckResult {
    if !status.healthy {
        return CheckResult::from(status);
    }
    if let Some(threshold) = threshold {
        if let Some(last_success) = status.last_success {
            if last_success.elapsed() > threshold {
                return CheckResult {
                    status: "down",
                    reason: Some("stale"),
                };
            }
        }
        // No last_success but healthy (e.g. freshly reported via startup) — not stale yet.
    }
    CheckResult::from(status)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// Handler for `GET /readyz`.
///
/// Reads the latest reported state from each subsystem reporter — no active
/// probing. Returns `200 OK` when all subsystems are up, `503` otherwise.
pub async fn readyz_handler(
    axum::extract::State(state): axum::extract::State<Arc<crate::types::AppState>>,
) -> axum::response::Response {
    let threshold = state.health.stale_threshold;

    let git = state.health.git.load();
    let embedding = state.health.embedding.load();
    let vector_index = state.health.vector_index.load();

    let git_check = check_with_staleness(&git, threshold);
    let embedding_check = check_with_staleness(&embedding, threshold);
    let vector_index_check = if state.index_mirror_is_intact() {
        check_with_staleness(&vector_index, threshold)
    } else {
        CheckResult {
            status: "down",
            reason: Some("mirror incomplete"),
        }
    };

    let sync_check = if state.health.require_sync {
        let sync = state.health.sync.load();
        Some(check_with_staleness(&sync, threshold))
    } else {
        None
    };

    let all_up = git_check.status == "up"
        && embedding_check.status == "up"
        && vector_index_check.status == "up"
        && sync_check.as_ref().is_none_or(|s| s.status == "up");

    let response = ReadyzResponse {
        status: if all_up { "ready" } else { "not_ready" },
        checks: ReadyzChecks {
            git_repo: git_check,
            embedding: embedding_check,
            vector_index: vector_index_check,
            sync: sync_check,
        },
    };

    // Transition-based logging via compare_exchange.
    let status_code = if all_up {
        if state
            .health
            .was_ready
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::info!("readyz: all subsystems up");
        }
        axum::http::StatusCode::OK
    } else {
        if state
            .health
            .was_ready
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::warn!(
                git_repo = response.checks.git_repo.status,
                embedding = response.checks.embedding.status,
                vector_index = response.checks.vector_index.status,
                sync = response.checks.sync.as_ref().map_or("n/a", |s| s.status),
                "readyz degraded: subsystem(s) down",
            );
        } else {
            tracing::debug!("readyz check: not ready");
        }
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };

    (status_code, axum::Json(response)).into_response()
}

/// Handler for `GET /healthz` (liveness probe — always 200 OK).
pub async fn healthz_handler() -> impl IntoResponse {
    axum::Json(serde_json::json!({"status": "ok"}))
}

/// Handler for `GET /version`.
pub async fn version_handler() -> impl IntoResponse {
    axum::Json(serde_json::json!({"version": env!("CARGO_PKG_VERSION")}))
}
