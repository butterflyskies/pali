//! Routes memory operations to the correct [`crate::repo::MemoryRepo`] based on scope.
//!
//! When per-scope remote mappings are configured, each mapped scope prefix
//! gets its own independent git repository. Unmapped scopes fall through to
//! the default repo. Read operations aggregate across all repos.

use std::sync::Arc;

use tracing::{info, warn};

use crate::{
    auth::AuthProvider,
    error::MemoryError,
    health::SubsystemReporter,
    repo::MemoryRepo,
    types::{ChangedMemories, Memory, MemoryName, PullResult, ResolvedChanges, Scope},
};

/// A scope-to-repo entry in the router.
#[derive(Clone)]
struct ScopeRoute {
    /// Scope prefix this route captures.
    prefix: String,
    /// The git repository for this scope.
    repo: Arc<MemoryRepo>,
    /// Branch name for push/pull (overrides the server-wide default).
    branch: Option<String>,
}

/// Routes memory operations to scope-specific git repositories.
///
/// Holds a default repo for unmapped scopes and zero or more scope-specific
/// repos. Write operations route to the repo that owns the scope. Read
/// operations aggregate across all repos.
#[derive(Clone)]
pub struct RepoRouter {
    /// The default repo for scopes that don't match any configured mapping.
    default_repo: Arc<MemoryRepo>,
    /// Scope-specific repos, ordered by prefix length descending so longest
    /// prefix matches first.
    routes: Vec<ScopeRoute>,
    /// Aggregate sync health reporter (#293 review, round 3).
    ///
    /// Each repo's pull/push reports per operation to a shared reporter, so
    /// with multiple repos the last operation's outcome would overwrite
    /// earlier failures. When present, [`RepoRouter::sync_all`] reports the
    /// aggregate outcome of the completed sync once, so readiness reflects
    /// every repo — not the last iteration.
    sync_reporter: Option<SubsystemReporter>,
    /// Aggregate git health reporter (#293 review, round 4).
    ///
    /// The same last-operation-wins hazard applies to local git health: in
    /// the skip-and-continue aggregate [`RepoRouter::list_memories`], a
    /// failed route followed by a clean repo would leave the shared reporter
    /// healthy while a scope's memories are missing from the aggregate. When
    /// present, the aggregate list settles the reporter once from the
    /// complete outcome.
    git_reporter: Option<SubsystemReporter>,
}

/// Result of a sync (push/pull) across all repos.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct MultiSyncResult {
    /// Per-repo sync outcomes, one entry per repo (default + each route).
    pub results: Vec<SyncEntry>,
}

impl MultiSyncResult {
    /// `true` when every repo's pull and push completed without error.
    pub fn all_ok(&self) -> bool {
        self.results.iter().all(SyncEntry::is_ok)
    }
}

/// Outcome of syncing a single repo.
#[derive(Debug)]
#[non_exhaustive]
pub struct SyncEntry {
    /// Human-readable label (`"default"` or the scope prefix).
    pub label: String,
    /// The scope used to resolve this repo (for reindex routing).
    pub scope: Scope,
    /// Pull result when the pull was performed and succeeded.
    pub pull: Option<PullResult>,
    /// Error message when the pull was attempted and failed.
    pub pull_error: Option<String>,
    /// Whether push succeeded. `true` in local-only mode (no remote means
    /// nothing to push); `false` when the push failed or was not attempted
    /// because the pull failed first.
    pub push_ok: bool,
    /// Error message when the push was attempted and failed.
    pub push_error: Option<String>,
    /// Memories that changed during pull (for incremental reindex).
    pub changes: Option<ChangedMemories>,
    /// Structured changed memories used by derived-index mirrors.
    pub(crate) resolved_changes: Option<ResolvedChanges>,
    /// Whether post-pull change discovery completed without gaps.
    pub(crate) changes_complete: bool,
}

impl SyncEntry {
    /// `true` when neither the pull nor the push recorded an error.
    pub fn is_ok(&self) -> bool {
        self.pull_error.is_none() && self.push_error.is_none()
    }

    /// Describe the recorded pull/push errors, or `None` when the entry
    /// completed without error.
    pub fn failure_summary(&self) -> Option<String> {
        let mut ops = Vec::new();
        if let Some(e) = &self.pull_error {
            ops.push(format!("pull failed: {e}"));
        }
        if let Some(e) = &self.push_error {
            ops.push(format!("push failed: {e}"));
        }
        if ops.is_empty() {
            None
        } else {
            Some(ops.join("; "))
        }
    }
}

impl RepoRouter {
    /// Create a router with only a default repo (no scope mappings).
    pub fn single(default_repo: Arc<MemoryRepo>) -> Self {
        Self {
            default_repo,
            routes: Vec::new(),
            sync_reporter: None,
            git_reporter: None,
        }
    }

    /// Create a router from config, initialising scope-specific repos.
    ///
    /// Each `RemoteMapping` in the config produces a new `MemoryRepo` at the
    /// resolved path with the mapping's URL as origin. Every mapped branch
    /// name is validated here (#293 review, round 3) — an invalid override
    /// must be rejected at construction, not discovered at the first
    /// push/pull.
    ///
    /// Repo paths are canonicalized before init and collisions are rejected
    /// (#293 review, round 4): two routes — or a route and the default repo —
    /// resolving to the same physical location would open one repo under
    /// separate mutexes, and the later init would rewrite `origin`, so
    /// nominally isolated scopes would share a tree and push to the
    /// last-configured remote.
    pub fn from_config(
        default_repo: Arc<MemoryRepo>,
        mappings: &[crate::config::RemoteMapping],
        git_reporter: &SubsystemReporter,
        sync_reporter: &SubsystemReporter,
    ) -> Result<Self, MemoryError> {
        let mut claimed: std::collections::HashMap<std::path::PathBuf, String> =
            std::collections::HashMap::new();
        claimed.insert(
            crate::fs_util::canonicalize_allow_missing(default_repo.root())?,
            "the default repository".to_string(),
        );

        let mut routes = Vec::with_capacity(mappings.len());
        for mapping in mappings {
            if let Some(branch) = &mapping.branch {
                crate::types::validate_branch_name(branch).map_err(|_| {
                    MemoryError::InvalidInput {
                        reason: format!(
                            "invalid branch '{}' for scope '{}'",
                            branch, mapping.scope
                        ),
                    }
                })?;
            }
            let path = crate::fs_util::canonicalize_allow_missing(&mapping.resolved_path()?)?;
            if let Some(owner) = claimed.get(&path) {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "repo path collision: scope '{}' resolves to '{}', which is already used by {}",
                        mapping.scope,
                        path.display(),
                        owner
                    ),
                });
            }
            claimed.insert(path.clone(), format!("scope '{}'", mapping.scope));
            info!(
                scope = %mapping.scope,
                path = %path.display(),
                url = %crate::repo::redact_url(&mapping.url),
                "initialising scope-specific repo"
            );
            let repo = MemoryRepo::init_or_open_with_reporter(
                &path,
                Some(&mapping.url),
                git_reporter.clone(),
                sync_reporter.clone(),
            )?;
            routes.push(ScopeRoute {
                prefix: mapping.scope.clone(),
                repo: Arc::new(repo),
                branch: mapping.branch.clone(),
            });
        }
        // Sort by prefix length descending so longest match wins.
        routes.sort_by_key(|r| std::cmp::Reverse(r.prefix.len()));
        Ok(Self {
            default_repo,
            routes,
            sync_reporter: Some(sync_reporter.clone()),
            git_reporter: Some(git_reporter.clone()),
        })
    }

    /// Find the repo that owns a given scope.
    fn repo_for_scope(&self, scope: &Scope) -> &Arc<MemoryRepo> {
        match scope {
            Scope::Root => &self.default_repo,
            Scope::Path(sp) => {
                let path = sp.as_str();
                for route in &self.routes {
                    if path == route.prefix
                        || (path.starts_with(&route.prefix)
                            && path.as_bytes().get(route.prefix.len()) == Some(&b'/'))
                    {
                        return &route.repo;
                    }
                }
                &self.default_repo
            }
        }
    }

    /// Resolve the branch name for a given scope.
    #[cfg(test)]
    fn branch_for_scope(&self, scope: &Scope, default_branch: &str) -> String {
        match scope {
            Scope::Root => default_branch.to_string(),
            Scope::Path(sp) => {
                let path = sp.as_str();
                for route in &self.routes {
                    if path == route.prefix
                        || (path.starts_with(&route.prefix)
                            && path.as_bytes().get(route.prefix.len()) == Some(&b'/'))
                    {
                        return route
                            .branch
                            .as_deref()
                            .unwrap_or(default_branch)
                            .to_string();
                    }
                }
                default_branch.to_string()
            }
        }
    }

    /// Get a reference to the default repo.
    pub fn default_repo(&self) -> &Arc<MemoryRepo> {
        &self.default_repo
    }

    /// Iterate over all repos (default + scope-mapped).
    fn all_repos(&self) -> impl Iterator<Item = (&str, &Arc<MemoryRepo>, Option<&str>, Scope)> {
        std::iter::once(("default", &self.default_repo, None, Scope::Root)).chain(
            self.routes.iter().map(|r| {
                let scope = crate::types::ScopePath::new(&r.prefix)
                    .map(Scope::Path)
                    .expect("scope prefix validated at config load");
                (r.prefix.as_str(), &r.repo, r.branch.as_deref(), scope)
            }),
        )
    }

    // -----------------------------------------------------------------------
    // Write operations — route to the correct repo
    // -----------------------------------------------------------------------

    /// Save a memory to the repo that owns its scope.
    pub async fn save_memory(&self, memory: &Memory) -> Result<(), MemoryError> {
        let repo = self.repo_for_scope(&memory.metadata.scope);
        repo.save_memory(memory).await
    }

    /// Delete a memory from the repo that owns its scope.
    pub async fn delete_memory(&self, name: &str, scope: &Scope) -> Result<(), MemoryError> {
        let repo = self.repo_for_scope(scope);
        repo.delete_memory(name, scope).await
    }

    /// Read a memory from the repo that owns its scope.
    pub async fn read_memory(&self, name: &str, scope: &Scope) -> Result<Memory, MemoryError> {
        let repo = self.repo_for_scope(scope);
        repo.read_memory(name, scope).await
    }

    /// Move a memory, possibly across repos.
    pub async fn move_memory(
        &self,
        source_name: &str,
        source_scope: &Scope,
        dest_name: &MemoryName,
        dest_scope: &Scope,
    ) -> Result<Memory, MemoryError> {
        let source_repo = self.repo_for_scope(source_scope);
        let dest_repo = self.repo_for_scope(dest_scope);

        if Arc::ptr_eq(source_repo, dest_repo) {
            // Same repo — use the atomic move.
            source_repo
                .move_memory(source_name, source_scope, dest_name, dest_scope)
                .await
        } else {
            // Cross-repo move: read from source, save to dest, delete from source.
            // Preserve id and created_at from the source so recall_log references
            // and memory identity survive the move.
            let source = source_repo.read_memory(source_name, source_scope).await?;
            let metadata = crate::types::MemoryMetadata {
                scope: dest_scope.clone(),
                tags: source.metadata.tags.clone(),
                source: source.metadata.source.clone(),
                created_at: source.metadata.created_at,
                updated_at: chrono::Utc::now(),
            };
            let dest = Memory::from_validated_with_id(
                source.id,
                dest_name.clone(),
                source.content.clone(),
                metadata,
            );
            dest_repo.save_memory(&dest).await?;
            if let Err(e) = source_repo.delete_memory(source_name, source_scope).await {
                warn!(
                    error = %e,
                    source = %source_name,
                    "cross-repo move: failed to delete source after successful save to destination — data exists in both repos"
                );
                return Err(e);
            }
            Ok(dest)
        }
    }

    // -----------------------------------------------------------------------
    // Read operations — aggregate across all repos
    // -----------------------------------------------------------------------

    /// `true` when `repo` is the repo that owns `scope` under the point-read
    /// routing rules (`repo_for_scope`).
    fn owns_scope(&self, repo: &Arc<MemoryRepo>, scope: &Scope) -> bool {
        Arc::ptr_eq(self.repo_for_scope(scope), repo)
    }

    /// List memories across all repos, filtered by scope.
    ///
    /// Each repo's results are filtered to the scopes that repo owns under
    /// the same routing rules as point reads: the default repo owns every
    /// scope not captured by a mapping, and each mapped repo owns exactly its
    /// subtree. Without this, a memory stranded in the wrong repo (e.g. a
    /// pre-existing `work/foo` in the default repo after `work` was mapped
    /// elsewhere) would appear in listings while `read` reports it not found,
    /// and duplicate `(scope, name)` keys could corrupt keyset pagination.
    pub async fn list_memories(&self, scope: Option<&Scope>) -> Result<Vec<Memory>, MemoryError> {
        if self.routes.is_empty() {
            return self.default_repo.list_memories(scope).await;
        }

        let mut all_memories = self.default_repo.list_memories(scope).await?;
        all_memories.retain(|m| self.owns_scope(&self.default_repo, &m.metadata.scope));
        let mut failed = 0usize;
        for route in &self.routes {
            match route.repo.list_memories(scope).await {
                Ok(memories) => all_memories.extend(
                    memories
                        .into_iter()
                        .filter(|m| self.owns_scope(&route.repo, &m.metadata.scope)),
                ),
                Err(e) => {
                    failed += 1;
                    warn!(
                        scope = %route.prefix,
                        error = %e,
                        "list_memories: failed to list from scope-specific repo; skipping"
                    );
                }
            }
        }
        // Settle aggregate git health once from the complete outcome
        // (#293 review, round 4). The per-operation reports are
        // last-operation-wins: a skipped failing route followed by a clean
        // repo would turn readiness green while a scope's memories are
        // missing from the aggregate.
        if let Some(reporter) = &self.git_reporter {
            if failed == 0 {
                reporter.report_ok();
            } else {
                reporter.report_err("one or more scope-specific repos failed to list");
            }
        }
        Ok(all_memories)
    }

    /// List memories across every repo, failing if any repo cannot be read.
    ///
    /// Rebuilds use this strict form because accepting a partial aggregate as
    /// git truth would silently erase the missing repo from a derived index.
    /// Ownership filtering applies here too: a memory stranded in a repo that
    /// does not own its scope is unreachable through point reads, so derived
    /// indexes must not serve it either.
    pub async fn list_memories_strict(&self) -> Result<Vec<Memory>, MemoryError> {
        let mut all_memories = self.default_repo.list_memories(None).await?;
        if !self.routes.is_empty() {
            all_memories.retain(|m| self.owns_scope(&self.default_repo, &m.metadata.scope));
        }
        for route in &self.routes {
            all_memories.extend(
                route
                    .repo
                    .list_memories(None)
                    .await?
                    .into_iter()
                    .filter(|m| self.owns_scope(&route.repo, &m.metadata.scope)),
            );
        }
        Ok(all_memories)
    }

    // -----------------------------------------------------------------------
    // Sync operations — push/pull each repo independently
    // -----------------------------------------------------------------------

    /// Sync all repos: pull then push each one.
    ///
    /// Errors on individual repos do not abort the remaining repos — a
    /// network blip on one remote must not block syncing the others. Each
    /// failure is recorded on that repo's [`SyncEntry`] (`pull_error` /
    /// `push_error`) so callers can surface partial failures instead of
    /// reporting a clean sync; check [`MultiSyncResult::all_ok`]. When the
    /// router carries a sync reporter, the aggregate outcome is reported
    /// once after all repos complete.
    pub async fn sync_all(
        &self,
        auth: &AuthProvider,
        default_branch: &str,
        pull_first: bool,
    ) -> Result<MultiSyncResult, MemoryError> {
        let mut result = MultiSyncResult::default();

        for (label, repo, branch_override, scope) in self.all_repos() {
            let branch = branch_override.unwrap_or(default_branch);
            let mut entry = SyncEntry {
                label: label.to_string(),
                scope,
                pull: None,
                pull_error: None,
                push_ok: false,
                push_error: None,
                changes: None,
                resolved_changes: None,
                changes_complete: true,
            };

            let mut has_remote = true;

            if pull_first {
                match repo.pull(auth, branch).await {
                    Ok(pull_result) => {
                        if matches!(pull_result, PullResult::NoRemote) {
                            has_remote = false;
                        }
                        // Compute changed memories for incremental reindex.
                        if let PullResult::FastForward {
                            old_head: oh,
                            new_head: nh,
                        }
                        | PullResult::Merged {
                            old_head: oh,
                            new_head: nh,
                            ..
                        } = &pull_result
                        {
                            let repo_clone = Arc::clone(repo);
                            let oh = *oh;
                            let nh = *nh;
                            match crate::repo::traced_spawn_blocking(move || {
                                let changes = repo_clone.diff_changed_memories(oh, nh)?;
                                let resolved = repo_clone.diff_changed_refs(oh, nh)?;
                                Ok::<_, MemoryError>((changes, resolved))
                            })
                            .await
                            {
                                Ok(Ok((changes, resolved))) => {
                                    if !changes.is_empty() {
                                        entry.changes = Some(changes);
                                    }
                                    if !resolved.is_empty() || resolved.unresolved > 0 {
                                        entry.resolved_changes = Some(resolved);
                                    }
                                }
                                Ok(Err(e)) => {
                                    entry.changes_complete = false;
                                    warn!(
                                        label = %label,
                                        error = %e,
                                        "sync: failed to diff changed memories"
                                    );
                                }
                                Err(e) => {
                                    entry.changes_complete = false;
                                    warn!(
                                        label = %label,
                                        error = %e,
                                        "sync: spawn_blocking failed for diff"
                                    );
                                }
                            }
                        }
                        entry.pull = Some(pull_result);
                    }
                    Err(e) => {
                        warn!(
                            label = %label,
                            error = %e,
                            "sync: pull failed — continuing with remaining repos"
                        );
                        entry.pull_error = Some(e.to_string());
                        result.results.push(entry);
                        continue;
                    }
                }
            }

            if has_remote {
                match repo.push(auth, branch).await {
                    Ok(()) => entry.push_ok = true,
                    Err(e) => {
                        warn!(
                            label = %label,
                            error = %e,
                            "sync: push failed — continuing with remaining repos"
                        );
                        entry.push_error = Some(e.to_string());
                    }
                }
            } else {
                entry.push_ok = true;
            }

            result.results.push(entry);
        }

        let failures: Vec<String> = result
            .results
            .iter()
            .filter_map(|e| {
                e.failure_summary()
                    .map(|summary| format!("{}: {summary}", e.label))
            })
            .collect();
        if !failures.is_empty() {
            warn!(
                failed = failures.len(),
                total = result.results.len(),
                "sync: {}/{} repos had errors: {}",
                failures.len(),
                result.results.len(),
                failures.join("; ")
            );
        }

        // Aggregate sync health, reported once from the completed result
        // (#293 review, round 3). The per-operation reports above are
        // last-operation-wins: a failed repo followed by a clean one would
        // leave the shared reporter healthy. The settled state must reflect
        // every repo's pull/push outcome.
        if let Some(reporter) = &self.sync_reporter {
            if result.all_ok() {
                reporter.report_ok();
            } else {
                reporter.report_err("one or more repos failed to sync");
            }
        }

        Ok(result)
    }

    /// Pull every repo — the default plus each scope-mapped route, respecting
    /// per-route branch overrides — without pushing (#328 review, round 2).
    ///
    /// This is the startup counterpart of [`RepoRouter::sync_all`]'s pull
    /// phase: on a fresh deployment the initial pull is what populates each
    /// repo, so all of them must be pulled before index freshness is decided,
    /// not just the default. A failure on one repo does not abort the rest.
    ///
    /// When the router carries a sync reporter, the aggregate outcome is
    /// settled once after every repo completes. The per-operation reports
    /// from [`MemoryRepo::pull`] are last-operation-wins, so a failed
    /// mapped-remote pull followed by a clean pull would otherwise leave
    /// shared sync health green while a scope's remote memories are absent.
    ///
    /// Returns `true` when every repo pulled cleanly.
    pub async fn pull_all(&self, auth: &AuthProvider, default_branch: &str) -> bool {
        let mut failures: Vec<String> = Vec::new();
        for (label, repo, branch_override, _scope) in self.all_repos() {
            let branch = branch_override.unwrap_or(default_branch);
            match repo.pull(auth, branch).await {
                Ok(result) => info!(label = %label, ?result, "initial pull completed"),
                Err(e) => {
                    warn!(
                        label = %label,
                        error = %e,
                        "initial pull failed — continuing with remaining repos"
                    );
                    failures.push(format!("{label}: {e}"));
                }
            }
        }
        if !failures.is_empty() {
            warn!(
                failed = failures.len(),
                "initial pull: {} repo(s) failed: {}",
                failures.len(),
                failures.join("; ")
            );
        }
        if let Some(reporter) = &self.sync_reporter {
            if failures.is_empty() {
                reporter.report_ok();
            } else {
                reporter.report_err("one or more repos failed the initial pull");
            }
        }
        failures.is_empty()
    }

    /// Get a composite HEAD SHA covering all repos.
    ///
    /// When no scope-specific routes are configured, returns the default repo's
    /// SHA directly (backward compatible). When routes exist, returns a
    /// deterministic string built from all repos' HEADs so that a change in
    /// *any* repo invalidates the stored index SHA and triggers a reindex.
    pub async fn head_sha(&self) -> Option<String> {
        if self.routes.is_empty() {
            return self.default_repo.head_sha().await;
        }

        // Collect (label, sha) from all repos. Labels are already unique
        // ("default" + each route prefix) and we sort for determinism.
        let mut parts: Vec<(String, String)> = Vec::with_capacity(1 + self.routes.len());

        if let Some(sha) = self.default_repo.head_sha().await {
            parts.push(("default".to_string(), sha));
        }
        for route in &self.routes {
            if let Some(sha) = route.repo.head_sha().await {
                parts.push((route.prefix.clone(), sha));
            }
        }

        if parts.is_empty() {
            return None;
        }

        parts.sort_by(|a, b| a.0.cmp(&b.0));

        let composite = parts
            .iter()
            .map(|(label, sha)| format!("{label}={sha}"))
            .collect::<Vec<_>>()
            .join(";");
        Some(composite)
    }

    /// Return a reference to the repo for a given scope (for direct access when needed).
    pub fn repo(&self, scope: &Scope) -> &Arc<MemoryRepo> {
        self.repo_for_scope(scope)
    }

    /// Check if there are any scope-specific routes configured.
    pub fn has_routes(&self) -> bool {
        !self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        search::{rebuild_lexical_from_router, LexicalIndex},
        types::{MemoryMetadata, MemoryName, ScopeFilter, ScopePath},
    };

    fn test_repo(dir: &tempfile::TempDir) -> Arc<MemoryRepo> {
        Arc::new(MemoryRepo::init_or_open(dir.path(), None).unwrap())
    }

    #[test]
    fn single_routes_everything_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let repo = test_repo(&dir);
        let router = RepoRouter::single(Arc::clone(&repo));

        assert!(Arc::ptr_eq(router.repo_for_scope(&Scope::Root), &repo));
        let sp = ScopePath::new("work").unwrap();
        assert!(Arc::ptr_eq(router.repo_for_scope(&Scope::Path(sp)), &repo));
    }

    #[test]
    fn routes_by_scope_prefix() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_repo = test_repo(&work_dir);

        let router = RepoRouter {
            default_repo: Arc::clone(&default_repo),
            routes: vec![ScopeRoute {
                prefix: "work".to_string(),
                repo: Arc::clone(&work_repo),
                branch: None,
            }],
            sync_reporter: None,
            git_reporter: None,
        };

        // Root goes to default.
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Root),
            &default_repo
        ));

        // "work" goes to work repo.
        let sp = ScopePath::new("work").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &work_repo
        ));

        // "work/subteam" goes to work repo (prefix match).
        let sp = ScopePath::new("work/subteam").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &work_repo
        ));

        // "personal" goes to default.
        let sp = ScopePath::new("personal").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &default_repo
        ));

        // "workflow" does NOT match "work" prefix (segment boundary).
        let sp = ScopePath::new("workflow").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &default_repo
        ));
    }

    #[test]
    fn longest_prefix_wins() {
        let default_dir = tempfile::tempdir().unwrap();
        let org_dir = tempfile::tempdir().unwrap();
        let org_team_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let org_repo = test_repo(&org_dir);
        let org_team_repo = test_repo(&org_team_dir);

        let mut routes = vec![
            ScopeRoute {
                prefix: "org".to_string(),
                repo: Arc::clone(&org_repo),
                branch: None,
            },
            ScopeRoute {
                prefix: "org/team".to_string(),
                repo: Arc::clone(&org_team_repo),
                branch: None,
            },
        ];
        routes.sort_by_key(|r| std::cmp::Reverse(r.prefix.len()));

        let router = RepoRouter {
            default_repo: Arc::clone(&default_repo),
            routes,
            sync_reporter: None,
            git_reporter: None,
        };

        // "org/team" matches the longer prefix.
        let sp = ScopePath::new("org/team").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &org_team_repo
        ));

        // "org/other" matches "org".
        let sp = ScopePath::new("org/other").unwrap();
        assert!(Arc::ptr_eq(
            router.repo_for_scope(&Scope::Path(sp)),
            &org_repo
        ));
    }

    #[test]
    fn branch_override() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_repo = test_repo(&work_dir);

        let router = RepoRouter {
            default_repo: Arc::clone(&default_repo),
            routes: vec![ScopeRoute {
                prefix: "work".to_string(),
                repo: Arc::clone(&work_repo),
                branch: Some("develop".to_string()),
            }],
            sync_reporter: None,
            git_reporter: None,
        };

        assert_eq!(router.branch_for_scope(&Scope::Root, "main"), "main");
        let sp = ScopePath::new("work").unwrap();
        assert_eq!(router.branch_for_scope(&Scope::Path(sp), "main"), "develop");
        let sp = ScopePath::new("other").unwrap();
        assert_eq!(router.branch_for_scope(&Scope::Path(sp), "main"), "main");
    }

    #[tokio::test]
    async fn lexical_rebuild_uses_default_and_mapped_repo_truth() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_repo = test_repo(&work_dir);
        let router = RepoRouter {
            default_repo,
            routes: vec![ScopeRoute {
                prefix: "work".to_string(),
                repo: work_repo,
                branch: None,
            }],
            sync_reporter: None,
            git_reporter: None,
        };

        let root = Memory::from_validated(
            MemoryName::new("root-note").unwrap(),
            "rootneedle".to_string(),
            MemoryMetadata::new(Scope::Root, vec![], None),
        );
        let work_scope = Scope::Path(ScopePath::new("work").unwrap());
        let work = Memory::from_validated(
            MemoryName::new("work-note").unwrap(),
            "mappedneedle".to_string(),
            MemoryMetadata::new(work_scope, vec![], None),
        );
        router.save_memory(&root).await.unwrap();
        router.save_memory(&work).await.unwrap();

        let lexical = Arc::new(LexicalIndex::new());
        let count = rebuild_lexical_from_router(&router, &lexical)
            .await
            .unwrap();
        assert_eq!(count, 2);

        let root_hits = lexical.search(&ScopeFilter::All, "rootneedle", 10).unwrap();
        assert_eq!(root_hits[0].0, root.mem_ref().qualified_path());
        let mapped_hits = lexical
            .search(&ScopeFilter::All, "mappedneedle", 10)
            .unwrap();
        assert_eq!(mapped_hits[0].0, work.mem_ref().qualified_path());
    }

    // -----------------------------------------------------------------------
    // Aggregate-read ownership (#293 review, round 2)
    //
    // Point reads route a scope exclusively to its owning repo; the
    // aggregate list operations must apply the same ownership rules, or a
    // memory stranded in the wrong repo (e.g. a pre-existing `work/foo` in
    // the default repo after `work` was mapped elsewhere) stays listed while
    // `read` reports it not found, and duplicate `(scope, name)` keys can
    // corrupt keyset pagination.
    // -----------------------------------------------------------------------

    /// Build a default+work router and return `(router, default_repo, work_repo)`.
    fn two_repo_router(
        default_dir: &tempfile::TempDir,
        work_dir: &tempfile::TempDir,
    ) -> (RepoRouter, Arc<MemoryRepo>, Arc<MemoryRepo>) {
        let default_repo = test_repo(default_dir);
        let work_repo = test_repo(work_dir);
        let router = RepoRouter {
            default_repo: Arc::clone(&default_repo),
            routes: vec![ScopeRoute {
                prefix: "work".to_string(),
                repo: Arc::clone(&work_repo),
                branch: None,
            }],
            sync_reporter: None,
            git_reporter: None,
        };
        (router, default_repo, work_repo)
    }

    fn memory_at(scope: &str, name: &str) -> Memory {
        let scope = if scope.is_empty() {
            Scope::Root
        } else {
            Scope::Path(ScopePath::new(scope).unwrap())
        };
        Memory::from_validated(
            MemoryName::new(name).unwrap(),
            format!("{name} content"),
            MemoryMetadata::new(scope, vec![], None),
        )
    }

    #[tokio::test]
    async fn list_excludes_memories_a_repo_does_not_own() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let (router, default_repo, work_repo) = two_repo_router(&default_dir, &work_dir);

        // Stranded: lives in the default repo under a scope the work repo
        // owns — the pre-existing-before-mapping case.
        let stranded = memory_at("work/foo", "stranded");
        default_repo.save_memory(&stranded).await.unwrap();
        // Stranded the other way: lives in the work repo under a scope the
        // default repo owns.
        let misplaced = memory_at("personal", "misplaced");
        work_repo.save_memory(&misplaced).await.unwrap();
        // Owned memories stay visible.
        let owned_work = memory_at("work/foo", "owned-work");
        router.save_memory(&owned_work).await.unwrap();
        let owned_default = memory_at("personal", "owned-default");
        router.save_memory(&owned_default).await.unwrap();

        let names = |memories: &[Memory]| {
            let mut names: Vec<String> = memories
                .iter()
                .map(|m| m.name.as_str().to_string())
                .collect();
            names.sort();
            names
        };

        let listed = router.list_memories(None).await.unwrap();
        assert_eq!(
            names(&listed),
            vec!["owned-default", "owned-work"],
            "aggregate list must exclude memories the holding repo does not own"
        );
        let strict = router.list_memories_strict().await.unwrap();
        assert_eq!(
            names(&strict),
            vec!["owned-default", "owned-work"],
            "strict aggregate must apply the same ownership filter"
        );

        // Coherence receipt: the excluded entries are exactly the ones point
        // reads cannot serve.
        let stranded_read = router
            .read_memory("stranded", &stranded.metadata.scope)
            .await;
        assert!(
            matches!(stranded_read, Err(MemoryError::NotFound { .. })),
            "read must not find the stranded memory: {stranded_read:?}"
        );
        let misplaced_read = router
            .read_memory("misplaced", &misplaced.metadata.scope)
            .await;
        assert!(
            matches!(misplaced_read, Err(MemoryError::NotFound { .. })),
            "read must not find the misplaced memory: {misplaced_read:?}"
        );
    }

    #[tokio::test]
    async fn list_yields_unique_scope_name_keys_when_both_repos_hold_the_key() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let (router, default_repo, _work_repo) = two_repo_router(&default_dir, &work_dir);

        // Same (scope, name) in both repos: the owner's copy must win and
        // the aggregate must not contain duplicate keys that would break
        // keyset pagination.
        let shadowed = Memory::from_validated(
            MemoryName::new("dup").unwrap(),
            "default copy".to_string(),
            MemoryMetadata::new(Scope::Path(ScopePath::new("work").unwrap()), vec![], None),
        );
        default_repo.save_memory(&shadowed).await.unwrap();
        let owned = Memory::from_validated(
            MemoryName::new("dup").unwrap(),
            "work copy".to_string(),
            MemoryMetadata::new(Scope::Path(ScopePath::new("work").unwrap()), vec![], None),
        );
        router.save_memory(&owned).await.unwrap();

        let listed = router.list_memories(None).await.unwrap();
        let dups: Vec<&Memory> = listed
            .iter()
            .filter(|m| m.mem_ref().qualified_path() == owned.mem_ref().qualified_path())
            .collect();
        assert_eq!(
            dups.len(),
            1,
            "aggregate must contain exactly one entry per (scope, name) key"
        );
        assert_eq!(
            dups[0].content, "work copy",
            "the owning repo's copy must be the one listed"
        );
    }

    #[tokio::test]
    async fn list_default_scopes_unaffected_by_ownership_filter() {
        let default_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let (router, _default_repo, _work_repo) = two_repo_router(&default_dir, &work_dir);

        let root = memory_at("", "root-note");
        router.save_memory(&root).await.unwrap();
        let personal = memory_at("personal", "personal-note");
        router.save_memory(&personal).await.unwrap();
        let work = memory_at("work", "work-note");
        router.save_memory(&work).await.unwrap();

        let all = router.list_memories(None).await.unwrap();
        assert_eq!(all.len(), 3, "normally-routed memories must all be listed");

        let root_only = router.list_memories(Some(&Scope::Root)).await.unwrap();
        let names: Vec<&str> = root_only.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["root-note"],
            "root-scope listing must be unaffected"
        );
    }

    // -----------------------------------------------------------------------
    // Mapped-branch validation (#293 review, round 3)
    //
    // Every configured branch override goes through `validate_branch_name`
    // at router construction — not just the server-wide default branch that
    // `--branch` validates in main.
    // -----------------------------------------------------------------------

    #[test]
    fn from_config_rejects_invalid_mapped_branch() {
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();

        for bad in [
            "bad..branch",
            "",
            "branch with space",
            "/leading",
            "trailing/",
        ] {
            let mapping = crate::config::RemoteMapping {
                scope: "work".to_string(),
                url: "file:///nonexistent/remote.git".to_string(),
                path: Some(work_dir.path().display().to_string()),
                branch: Some(bad.to_string()),
            };
            let result = RepoRouter::from_config(
                Arc::clone(&default_repo),
                std::slice::from_ref(&mapping),
                &health.git,
                &health.sync,
            );
            match result {
                Err(MemoryError::InvalidInput { .. }) => {}
                Err(other) => panic!("branch {bad:?} must fail as InvalidInput: {other:?}"),
                Ok(_) => panic!("branch {bad:?} must be rejected at construction"),
            }
        }

        // A valid override still constructs, proving the gate rejects the
        // name, not the presence of an override.
        let mapping = crate::config::RemoteMapping {
            scope: "work".to_string(),
            url: "file:///nonexistent/remote.git".to_string(),
            path: Some(work_dir.path().display().to_string()),
            branch: Some("release/1.0".to_string()),
        };
        RepoRouter::from_config(
            default_repo,
            std::slice::from_ref(&mapping),
            &health.git,
            &health.sync,
        )
        .expect("a valid mapped branch must construct");
    }

    // -----------------------------------------------------------------------
    // Aggregate sync health (#293 review, round 3)
    //
    // Each repo's pull/push reports per operation to the shared sync
    // reporter, so multi-repo readiness was last-operation-wins: a failed
    // repo followed by a clean one left the reporter healthy. `sync_all`
    // must settle the reporter once from the completed `MultiSyncResult`.
    // -----------------------------------------------------------------------

    fn sync_test_auth() -> AuthProvider {
        AuthProvider::with_token("ghp_fake_token")
    }

    /// Create a bare origin seeded with one root-scope memory via a writer
    /// clone, so a fresh repo can pull and push it cleanly.
    async fn seeded_remote(seed_name: &str) -> (tempfile::TempDir, String) {
        let remote_dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote_dir.path()).unwrap();
        let url = format!("file://{}", remote_dir.path().display());
        let writer_dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(MemoryRepo::init_or_open(writer_dir.path(), Some(&url)).unwrap());
        writer.save_memory(&memory_at("", seed_name)).await.unwrap();
        writer.push(&sync_test_auth(), "main").await.unwrap();
        (remote_dir, url)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_health_reflects_aggregate_not_last_repo() {
        // The default repo — FIRST in sync order — has an unreachable
        // origin, so its pull fails. The mapped repo — LAST — syncs
        // cleanly, so per-operation reporting alone would leave the shared
        // reporter healthy.
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = Arc::new(
            MemoryRepo::init_or_open(
                default_dir.path(),
                Some("file:///nonexistent/pali-test-remote.git"),
            )
            .unwrap(),
        );

        let (_remote_dir, url) = seeded_remote("seed").await;
        let work_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();
        let mapping = crate::config::RemoteMapping {
            scope: "work".to_string(),
            url,
            path: Some(work_dir.path().display().to_string()),
            branch: None,
        };
        let router = RepoRouter::from_config(
            default_repo,
            std::slice::from_ref(&mapping),
            &health.git,
            &health.sync,
        )
        .unwrap();

        let result = router
            .sync_all(&sync_test_auth(), "main", true)
            .await
            .unwrap();
        assert!(!result.all_ok());
        assert!(
            result.results[0].pull_error.is_some(),
            "precondition: the first repo's pull must fail"
        );
        assert!(
            result.results[1].is_ok(),
            "precondition: the last repo must sync cleanly"
        );

        let sync = health.sync.load();
        assert!(
            !sync.healthy,
            "aggregate sync health must reflect the failed repo, not the \
             last repo's clean operations"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_health_ok_when_every_repo_syncs() {
        let (_default_remote, default_url) = seeded_remote("seed-default").await;
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo =
            Arc::new(MemoryRepo::init_or_open(default_dir.path(), Some(&default_url)).unwrap());

        let (_work_remote, work_url) = seeded_remote("seed-work").await;
        let work_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();
        let mapping = crate::config::RemoteMapping {
            scope: "work".to_string(),
            url: work_url,
            path: Some(work_dir.path().display().to_string()),
            branch: None,
        };
        let router = RepoRouter::from_config(
            default_repo,
            std::slice::from_ref(&mapping),
            &health.git,
            &health.sync,
        )
        .unwrap();

        let result = router
            .sync_all(&sync_test_auth(), "main", true)
            .await
            .unwrap();
        assert!(
            result.all_ok(),
            "precondition: every repo must sync cleanly"
        );
        assert!(
            health.sync.load().healthy,
            "a fully clean sync must settle the reporter healthy"
        );
    }

    // -----------------------------------------------------------------------
    // Startup pull-all aggregate health (#328 review, round 2)
    //
    // `pull_all` is the startup counterpart of `sync_all`'s pull phase and
    // carries the same last-operation-wins hazard: a failed mapped-remote
    // pull followed by a clean pull would leave the shared sync reporter
    // healthy while a scope's remote memories are absent. The router must
    // settle sync health once from the complete outcome.
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_pull_health_reflects_failed_mapped_remote_not_last_pull() {
        let (_default_remote, default_url) = seeded_remote("seed-default").await;
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo =
            Arc::new(MemoryRepo::init_or_open(default_dir.path(), Some(&default_url)).unwrap());

        // Route order is prefix-length descending, so "broken" (unreachable
        // remote) pulls BEFORE "work" (clean): the last per-operation report
        // is the clean pull, and only the aggregate settle keeps readiness
        // honest.
        let (_work_remote, work_url) = seeded_remote("seed-work").await;
        let broken_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();
        let mappings = vec![
            crate::config::RemoteMapping {
                scope: "broken".to_string(),
                url: "file:///nonexistent/pali-test-remote.git".to_string(),
                path: Some(broken_dir.path().display().to_string()),
                branch: None,
            },
            crate::config::RemoteMapping {
                scope: "work".to_string(),
                url: work_url,
                path: Some(work_dir.path().display().to_string()),
                branch: None,
            },
        ];
        let router =
            RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync).unwrap();

        let all_ok = router.pull_all(&sync_test_auth(), "main").await;
        assert!(!all_ok, "pull_all must report the failed mapped remote");
        assert!(
            !health.sync.load().healthy,
            "aggregate sync health must reflect the failed mapped-remote \
             pull, not the last repo's clean pull"
        );
    }

    // -----------------------------------------------------------------------
    // Repo-path collisions (#293 review, round 4)
    //
    // Two routes — or a route and the default repo — resolving to the same
    // physical location open one repo under separate mutexes, and the later
    // init rewrites `origin`, so nominally isolated scopes share a tree and
    // push to the last-configured remote. Construction must reject every
    // colliding spelling: explicit duplicates, symlink aliases, and paths
    // aliasing the default repo.
    // -----------------------------------------------------------------------

    fn mapping_at(scope: &str, path: &std::path::Path) -> crate::config::RemoteMapping {
        crate::config::RemoteMapping {
            scope: scope.to_string(),
            url: format!("https://example.com/{}.git", scope.replace('/', "-")),
            path: Some(path.display().to_string()),
            branch: None,
        }
    }

    fn expect_collision(result: Result<RepoRouter, MemoryError>, case: &str) {
        match result {
            Err(MemoryError::InvalidInput { reason }) => {
                assert!(
                    reason.contains("collision"),
                    "{case}: error must name the collision: {reason}"
                );
            }
            Err(other) => panic!("{case}: must fail as InvalidInput: {other:?}"),
            Ok(_) => panic!("{case}: colliding repo paths must be rejected at construction"),
        }
    }

    #[test]
    fn from_config_rejects_explicit_path_collision() {
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let shared_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();

        let mappings = vec![
            mapping_at("work", shared_dir.path()),
            mapping_at("play", shared_dir.path()),
        ];
        expect_collision(
            RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync),
            "two scopes with the same explicit path",
        );
    }

    #[test]
    fn from_config_rejects_mapped_path_aliasing_default_repo() {
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let health = crate::health::HealthRegistry::new();

        let mappings = vec![mapping_at("work", default_dir.path())];
        expect_collision(
            RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync),
            "a mapped path equal to the default repo",
        );
    }

    #[cfg(unix)]
    #[test]
    fn from_config_rejects_symlink_alias_collision() {
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = base.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let health = crate::health::HealthRegistry::new();

        let mappings = vec![mapping_at("work", &real), mapping_at("play", &alias)];
        expect_collision(
            RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync),
            "a symlink alias of an already-claimed path",
        );
    }

    #[cfg(unix)]
    #[test]
    fn from_config_rejects_symlink_dot_dot_alias_of_default_repo() {
        // Syne's round-5 repro: base/link -> else/dir, default repo opened
        // from the raw spelling base/link/../repo. Real traversal opens
        // else/repo (`..` resolves against the symlink TARGET), but a
        // lexical normalization would record base/repo as the collision
        // key — so a mapped route explicitly targeting else/repo would pass
        // collision detection, open the same physical repo under a second
        // mutex, and rewrite its origin. Construction must fail.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let else_dir = tmp.path().join("else");
        std::fs::create_dir_all(else_dir.join("dir")).unwrap();
        std::fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(else_dir.join("dir"), base.join("link")).unwrap();

        // Open the default repo from the raw aliased spelling, as a caller
        // that skipped canonicalization would.
        let raw = base.join("link/../repo");
        let default_repo = Arc::new(MemoryRepo::init_or_open(&raw, None).unwrap());
        // Precondition for the repro: the raw spelling and the mapped
        // target are the same physical directory.
        assert_eq!(
            std::fs::canonicalize(&raw).unwrap(),
            std::fs::canonicalize(else_dir.join("repo")).unwrap(),
            "precondition: raw spelling must traverse to else/repo"
        );
        let health = crate::health::HealthRegistry::new();

        let mappings = vec![mapping_at("work", &else_dir.join("repo"))];
        expect_collision(
            RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync),
            "a symlink+`..` spelling of the default repo",
        );
    }

    #[test]
    fn from_config_accepts_distinct_paths() {
        // Guard against the collision check over-rejecting: distinct
        // physical locations must still construct.
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_dir = tempfile::tempdir().unwrap();
        let play_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();

        let mappings = vec![
            mapping_at("work", work_dir.path()),
            mapping_at("play", play_dir.path()),
        ];
        RepoRouter::from_config(default_repo, &mappings, &health.git, &health.sync)
            .expect("distinct repo paths must construct");
    }

    // -----------------------------------------------------------------------
    // Credential redaction in router-init logging (#293 review, round 4)
    //
    // `from_config` logged the raw mapping URL at INFO, so credential-bearing
    // HTTPS userinfo bypassed the `redact_url` guard that repo-level logging
    // already applies.
    // -----------------------------------------------------------------------

    #[test]
    fn from_config_logs_redact_mapped_credentials() {
        let default_dir = tempfile::tempdir().unwrap();
        let default_repo = test_repo(&default_dir);
        let work_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();
        let mapping = crate::config::RemoteMapping {
            scope: "work".to_string(),
            url: "https://user:ghp_supersecret123@example.com/org/repo.git".to_string(),
            path: Some(work_dir.path().display().to_string()),
            branch: None,
        };

        let logs = crate::test_log::capture_info_logs(|| {
            RepoRouter::from_config(
                default_repo,
                std::slice::from_ref(&mapping),
                &health.git,
                &health.sync,
            )
            .expect("router must construct");
        });

        assert!(
            !logs.contains("ghp_supersecret123"),
            "the credential must never reach the logs: {logs}"
        );
        assert!(
            logs.contains("[REDACTED]"),
            "the remote URL must still be logged in redacted form: {logs}"
        );
    }

    // -----------------------------------------------------------------------
    // Aggregate git health (#293 review, round 4)
    //
    // All repos share one git reporter, and each repo reports per operation.
    // In the skip-and-continue aggregate `list_memories`, a failed route
    // followed by a clean repo left the shared reporter healthy while a
    // scope's memories were missing from the aggregate. The router must
    // settle git health once from the complete outcome, mirroring the
    // round-3 `sync_all` treatment.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn aggregate_list_git_health_reflects_failed_route_not_last_operation() {
        let default_dir = tempfile::tempdir().unwrap();
        let broken_dir = tempfile::tempdir().unwrap();
        let ok_dir = tempfile::tempdir().unwrap();
        let health = crate::health::HealthRegistry::new();
        let shared_repo = |dir: &tempfile::TempDir| {
            Arc::new(
                MemoryRepo::init_or_open_with_reporter(
                    dir.path(),
                    None,
                    health.git.clone(),
                    health.sync.clone(),
                )
                .unwrap(),
            )
        };
        let default_repo = shared_repo(&default_dir);
        let broken_repo = shared_repo(&broken_dir);
        let ok_repo = shared_repo(&ok_dir);

        // Sabotage the broken repo: a regular file where the `projects`
        // directory belongs makes the unscoped list fail (read_dir on a
        // file), independent of filesystem permissions.
        let sabotage = broken_dir.path().join("projects");
        std::fs::write(&sabotage, "not a directory").unwrap();

        // Order matters: the broken route lists BEFORE the ok route, so the
        // last per-operation report is the ok route's clean list.
        let router = RepoRouter {
            default_repo,
            routes: vec![
                ScopeRoute {
                    prefix: "broken".to_string(),
                    repo: broken_repo,
                    branch: None,
                },
                ScopeRoute {
                    prefix: "okay".to_string(),
                    repo: ok_repo,
                    branch: None,
                },
            ],
            sync_reporter: None,
            git_reporter: Some(health.git.clone()),
        };

        // The aggregate itself succeeds (skip-and-continue is intentional) …
        router
            .list_memories(None)
            .await
            .expect("aggregate list skips the failed route");
        // … but readiness must reflect the skipped repo, not the clean
        // operation that happened to run last.
        assert!(
            !health.git.load().healthy,
            "git health must reflect the failed route, not the last \
             repo's clean list"
        );

        // Once the failed route recovers, the next aggregate settles the
        // reporter healthy again.
        std::fs::remove_file(&sabotage).unwrap();
        router
            .list_memories(None)
            .await
            .expect("repaired aggregate list succeeds");
        assert!(
            health.git.load().healthy,
            "a fully clean aggregate must settle the reporter healthy"
        );
    }
}
