//! A bounded session manager for the Streamable HTTP server.
//!
//! [`BoundedSessionManager`](crate::session_cap::BoundedSessionManager) wraps
//! rmcp's `LocalSessionManager` and adds:
//!
//! - **a live-session cap** with FIFO eviction: when the cap is reached, the
//!   oldest session (by creation order) is closed before the new one is made;
//! - **an optional rate limit** on session creation (a sliding window); a
//!   creation over the limit is refused with
//!   [`BoundedSessionError::RateLimited`](crate::session_cap::BoundedSessionError::RateLimited)
//!   and evicts nothing;
//! - **an idle timeout**, passed through to rmcp as `SessionConfig::keep_alive`;
//! - **an optional maximum lifetime**, after which a session is closed whatever
//!   its activity.
//!
//! Each creation logs `session created` and each close logs `session closed`
//! with `duration_secs` and a `reason` of `Evicted`, `MaxLifetime` or `Closed`.
//!
//! Only sessions opened by `initialize` pass through this manager. With rmcp's
//! default `legacy_session_mode`, a request on the 2026-07-28 protocol
//! revision is served stateless and never reaches it, so the cap, the rate
//! limit, the idle timeout and the maximum lifetime do not apply to it.
//!
//! This is a port of `mcp-session` 0.2.0 (MIT OR Apache-2.0), whose newest
//! release requires rmcp 1; Pali carries it in-tree to move to rmcp 3
//! (lacuna/pali#421). Behaviour is kept as it was in that crate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_core::Stream;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError, LocalSessionWorker, SessionConfig,
};
use rmcp::transport::streamable_http_server::session::{
    ServerSseMessage, SessionId, SessionManager,
};
use rmcp::transport::WorkerTransport;

/// Errors returned by [`BoundedSessionManager`].
#[derive(Debug, thiserror::Error)]
pub enum BoundedSessionError {
    /// Propagated from the inner [`LocalSessionManager`].
    #[error(transparent)]
    Inner(#[from] LocalSessionManagerError),
    /// Session creation was refused because the rate limit was exceeded.
    #[error("session creation rate limit exceeded")]
    RateLimited,
}

/// Why a session was closed; logged as the `reason` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    /// Evicted to make room for a new session (FIFO).
    Evicted,
    /// The session reached its configured maximum lifetime.
    MaxLifetime,
    /// Closed explicitly (client DELETE, or the session's service ended).
    Closed,
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Evicted => f.write_str("Evicted"),
            Self::MaxLifetime => f.write_str("MaxLifetime"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

struct SessionLifecycleEntry {
    created_at: Instant,
    abort_handle: Option<tokio::task::AbortHandle>,
}

/// Sliding-window rate limiter for session creation.
struct RateLimiter {
    max_creates: usize,
    window: Duration,
    tracker: tokio::sync::Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    fn new(max_creates: usize, window: Duration) -> Self {
        Self {
            max_creates,
            window,
            tracker: tokio::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Reserve a slot, or refuse if the window is full. A caller whose
    /// creation then fails must [`rollback`](Self::rollback) the slot.
    async fn reserve(&self) -> Result<Instant, BoundedSessionError> {
        let mut tracker = self.tracker.lock().await;
        let now = Instant::now();
        while tracker
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            tracker.pop_front();
        }
        if tracker.len() >= self.max_creates {
            return Err(BoundedSessionError::RateLimited);
        }
        tracker.push_back(now);
        Ok(now)
    }

    /// Return a slot reserved at `reserved_at`. Under interleaving a later
    /// reservation may follow ours, so the entry is not necessarily last.
    async fn rollback(&self, reserved_at: Instant) {
        let mut tracker = self.tracker.lock().await;
        if let Some(pos) = tracker.iter().rposition(|t| *t == reserved_at) {
            tracker.remove(pos);
        }
    }
}

/// Wraps [`LocalSessionManager`] and caps the number of live sessions.
///
/// Built with [`BoundedSessionManagerBuilder`].
///
/// # Concurrency note
///
/// Under concurrent session creation the live count may transiently exceed
/// `max_sessions` by at most the number of concurrent callers: the cap is
/// best-effort under contention, as it was in `mcp-session`.
pub struct BoundedSessionManager {
    inner: LocalSessionManager,
    max_sessions: usize,
    /// Session IDs in creation order, for FIFO eviction.
    creation_order: tokio::sync::Mutex<VecDeque<SessionId>>,
    rate_limiter: Option<RateLimiter>,
    max_lifetime: Option<Duration>,
    lifecycle: tokio::sync::RwLock<HashMap<SessionId, SessionLifecycleEntry>>,
    /// Lets the max-lifetime timer call back into the manager.
    self_ref: Weak<Self>,
}

impl BoundedSessionManager {
    /// The number of sessions this manager is tracking as live.
    #[cfg(test)]
    async fn active_session_count(&self) -> usize {
        self.lifecycle.read().await.len()
    }

    /// One close path: drop the lifecycle entry and its timer, close the inner
    /// session, update the creation order, and log the reason.
    async fn close_session_impl(
        &self,
        id: &SessionId,
        reason: CloseReason,
    ) -> Result<(), BoundedSessionError> {
        let entry = self.lifecycle.write().await.remove(id);
        if let Some(handle) = entry.as_ref().and_then(|e| e.abort_handle.as_ref()) {
            handle.abort();
        }
        let elapsed = entry.as_ref().map(|e| e.created_at.elapsed());

        let close_result = self.inner.close_session(id).await;

        self.creation_order.lock().await.retain(|s| s != id);

        if let Some(elapsed) = elapsed {
            tracing::info!(
                session_id = %id,
                duration_secs = elapsed.as_secs_f64(),
                reason = %reason,
                "session closed"
            );
        }

        close_result.map_err(Into::into)
    }
}

impl SessionManager for BoundedSessionManager {
    type Error = BoundedSessionError;
    type Transport = WorkerTransport<LocalSessionWorker>;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let rate_reserved_at = match &self.rate_limiter {
            Some(limiter) => Some(limiter.reserve().await?),
            None => None,
        };

        // The inner map is the authoritative live count. A session whose
        // worker has ended (idle timeout included) is closed through our
        // `close_session` once rmcp's serve task sees it end, which also drops
        // it from the deque; counting the map keeps the short window before
        // that from skewing the cap.
        let evict_candidate = {
            let order = self.creation_order.lock().await;
            let live_count = self.inner.sessions.read().await.len();
            if live_count >= self.max_sessions {
                order.front().cloned()
            } else {
                None
            }
        };

        if let Some(oldest) = &evict_candidate {
            let _ = self.close_session_impl(oldest, CloseReason::Evicted).await;
        }

        let result = self.inner.create_session().await;
        if result.is_err() {
            if let (Some(limiter), Some(reserved_at)) = (&self.rate_limiter, rate_reserved_at) {
                limiter.rollback(reserved_at).await;
            }
        }
        let (id, transport) = result?;

        {
            let mut order = self.creation_order.lock().await;
            if let Some(oldest) = &evict_candidate {
                order.retain(|s| s != oldest);
            }
            // Drop deque entries for sessions no longer in the inner map: the
            // short window before rmcp's close of an ended session reaches us.
            let live_ids: HashSet<SessionId> =
                self.inner.sessions.read().await.keys().cloned().collect();
            order.retain(|s| live_ids.contains(s));
            order.push_back(id.clone());
        }

        let abort_handle = self.max_lifetime.map(|max_lifetime| {
            let weak = self.self_ref.clone();
            let timer_id = id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(max_lifetime).await;
                if let Some(manager) = weak.upgrade() {
                    let _ = manager
                        .close_session_impl(&timer_id, CloseReason::MaxLifetime)
                        .await;
                }
            })
            .abort_handle()
        });

        self.lifecycle.write().await.insert(
            id.clone(),
            SessionLifecycleEntry {
                created_at: Instant::now(),
                abort_handle,
            },
        );

        tracing::info!(session_id = %id, "session created");

        Ok((id, transport))
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.close_session_impl(id, CloseReason::Closed).await
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.inner.has_session(id).await?)
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        Ok(self.inner.create_stream(id, message).await?)
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        Ok(self.inner.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        Ok(self.inner.create_standalone_stream(id).await?)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        Ok(self.inner.resume(id, last_event_id).await?)
    }
}

/// Builder for [`BoundedSessionManager`].
pub struct BoundedSessionManagerBuilder {
    max_sessions: usize,
    idle_timeout: Option<Duration>,
    max_lifetime: Option<Duration>,
    rate_limit: Option<(usize, Duration)>,
}

impl BoundedSessionManagerBuilder {
    /// Start a builder capped at `max_sessions` live sessions.
    ///
    /// # Panics
    ///
    /// Panics if `max_sessions` is 0.
    pub fn new(max_sessions: usize) -> Self {
        assert!(max_sessions >= 1, "max_sessions must be at least 1, got 0");
        Self {
            max_sessions,
            idle_timeout: None,
            max_lifetime: None,
            rate_limit: None,
        }
    }

    /// Close a session after this long without activity
    /// (`SessionConfig::keep_alive`). Unset, rmcp's default applies.
    #[must_use]
    pub fn idle_timeout(mut self, duration: Duration) -> Self {
        self.idle_timeout = Some(duration);
        self
    }

    /// Close a session this long after creation, whatever its activity.
    /// [`build`](Self::build) must then run inside a Tokio runtime.
    #[must_use]
    pub fn max_lifetime(mut self, duration: Duration) -> Self {
        self.max_lifetime = Some(duration);
        self
    }

    /// Allow at most `max_creates` session creations in any rolling `window`.
    ///
    /// # Panics
    ///
    /// Panics if `max_creates` is 0.
    #[must_use]
    pub fn rate_limit(mut self, max_creates: usize, window: Duration) -> Self {
        assert!(
            max_creates >= 1,
            "max_creates must be at least 1; pass no rate limit instead of 0"
        );
        self.rate_limit = Some((max_creates, window));
        self
    }

    /// Build the manager.
    pub fn build(self) -> Arc<BoundedSessionManager> {
        let mut session_config = SessionConfig::default();
        if let Some(idle) = self.idle_timeout {
            session_config.keep_alive = Some(idle);
        }
        let rate_limiter = self
            .rate_limit
            .map(|(max_creates, window)| RateLimiter::new(max_creates, window));

        Arc::new_cyclic(|weak| {
            let mut inner = LocalSessionManager::default();
            inner.session_config = session_config;
            BoundedSessionManager {
                inner,
                max_sessions: self.max_sessions,
                creation_order: tokio::sync::Mutex::new(VecDeque::new()),
                rate_limiter,
                max_lifetime: self.max_lifetime,
                lifecycle: tokio::sync::RwLock::new(HashMap::new()),
                self_ref: weak.clone(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn under_the_cap_every_session_stays_live() {
        let manager = BoundedSessionManagerBuilder::new(3).build();
        let (a, _ta) = manager.create_session().await.unwrap();
        let (b, _tb) = manager.create_session().await.unwrap();

        assert_eq!(manager.active_session_count().await, 2);
        assert!(manager.has_session(&a).await.unwrap());
        assert!(manager.has_session(&b).await.unwrap());
    }

    #[tokio::test]
    async fn at_the_cap_the_oldest_session_is_evicted() {
        let manager = BoundedSessionManagerBuilder::new(2).build();
        let (a, _ta) = manager.create_session().await.unwrap();
        let (b, _tb) = manager.create_session().await.unwrap();
        // The cap is reached: the next creation succeeds and evicts `a`.
        let (c, _tc) = manager.create_session().await.unwrap();

        assert_eq!(manager.active_session_count().await, 2);
        assert!(!manager.has_session(&a).await.unwrap(), "oldest evicted");
        assert!(manager.has_session(&b).await.unwrap());
        assert!(manager.has_session(&c).await.unwrap());

        // Eviction is FIFO: the next one out is `b`, not the newer `c`.
        let (d, _td) = manager.create_session().await.unwrap();
        assert!(!manager.has_session(&b).await.unwrap());
        assert!(manager.has_session(&c).await.unwrap());
        assert!(manager.has_session(&d).await.unwrap());
    }

    #[tokio::test]
    async fn a_closed_session_frees_its_slot() {
        let manager = BoundedSessionManagerBuilder::new(2).build();
        let (a, _ta) = manager.create_session().await.unwrap();
        let (b, _tb) = manager.create_session().await.unwrap();

        manager.close_session(&a).await.unwrap();
        assert_eq!(manager.active_session_count().await, 1);

        // The freed slot is used: `b` survives the next creation.
        let (c, _tc) = manager.create_session().await.unwrap();
        assert_eq!(manager.active_session_count().await, 2);
        assert!(manager.has_session(&b).await.unwrap(), "no eviction");
        assert!(manager.has_session(&c).await.unwrap());
    }

    #[tokio::test]
    async fn over_the_rate_limit_creation_is_refused_without_eviction() {
        let manager = BoundedSessionManagerBuilder::new(1)
            .rate_limit(1, Duration::from_secs(60))
            .build();
        let (a, _ta) = manager.create_session().await.unwrap();

        let refused = manager.create_session().await;
        assert!(matches!(refused, Err(BoundedSessionError::RateLimited)));
        assert!(manager.has_session(&a).await.unwrap(), "nothing evicted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_is_closed_at_its_max_lifetime() {
        let manager = BoundedSessionManagerBuilder::new(2)
            .max_lifetime(Duration::from_secs(10))
            .build();
        let (a, _ta) = manager.create_session().await.unwrap();

        tokio::time::sleep(Duration::from_secs(11)).await;

        assert!(!manager.has_session(&a).await.unwrap());
        assert_eq!(manager.active_session_count().await, 0);
    }
}
