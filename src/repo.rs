use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use git2::{build::CheckoutBuilder, ErrorCode, MergeOptions, Repository, Signature};
use tracing::{info, warn};

use secrecy::{ExposeSecret, SecretString};

use crate::{
    auth::AuthProvider,
    error::MemoryError,
    health::SubsystemReporter,
    types::{
        ChangedMemories, Memory, MemoryMetadata, MemoryName, PullResult, ResolvedChanges, Scope,
    },
};

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

/// Strip userinfo (credentials) from a URL before logging.
///
/// `https://user:token@host/path` → `https://[REDACTED]@host/path`
pub(crate) fn redact_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_at = &url[at_pos + 1..];
            return format!("{}[REDACTED]@{}", scheme, after_at);
        }
    }
    url.to_string()
}

/// Return the current HEAD commit OID as a 20-byte array.
///
/// Returns `[0u8; 20]` as a sentinel when the branch is unborn (no commits yet).
fn capture_head_oid(repo: &git2::Repository) -> Result<[u8; 20], MemoryError> {
    match repo.head() {
        Ok(h) => {
            let oid = h.peel_to_commit()?.id();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(oid.as_bytes());
            Ok(buf)
        }
        // Unborn branch — use zero OID as sentinel.
        Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => {
            Ok([0u8; 20])
        }
        Err(e) => Err(MemoryError::Git(e)),
    }
}

/// Perform a fast-forward of `fetch_commit` into `branch`.
///
/// Captures the old HEAD OID (zero sentinel if unborn), advances the branch
/// ref, sets HEAD, and force-checks out the new tree.
fn fast_forward(
    repo: &git2::Repository,
    fetch_commit: &git2::AnnotatedCommit,
    branch: &str,
) -> Result<PullResult, MemoryError> {
    let old_head = capture_head_oid(repo)?;

    let refname = format!("refs/heads/{branch}");
    let target_oid = fetch_commit.id();

    match repo.find_reference(&refname) {
        Ok(mut reference) => {
            reference.set_target(target_oid, &format!("pull: fast-forward to {}", target_oid))?;
        }
        Err(e) if e.code() == ErrorCode::NotFound => {
            // Branch doesn't exist locally yet — create it.
            repo.reference(
                &refname,
                target_oid,
                true,
                &format!("pull: create branch {} from fetch", branch),
            )?;
        }
        Err(e) => return Err(MemoryError::Git(e)),
    }

    repo.set_head(&refname)?;
    let mut checkout = CheckoutBuilder::default();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))?;

    let mut new_head = [0u8; 20];
    new_head.copy_from_slice(target_oid.as_bytes());

    info!("pull: fast-forwarded to {}", target_oid);
    Ok(PullResult::FastForward { old_head, new_head })
}

/// Build a `RemoteCallbacks` that authenticates with the given token.
///
/// The callbacks live for `'static` because the token is moved in.
fn build_auth_callbacks(token: SecretString) -> git2::RemoteCallbacks<'static> {
    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.credentials(move |_url, _username, _allowed| {
        git2::Cred::userpass_plaintext("x-access-token", token.expose_secret())
    });
    callbacks
}

/// Spawn a blocking task with the caller's `tracing::Dispatch` propagated.
///
/// Blocking-pool threads do not inherit the caller's thread-local dispatch,
/// so `Span::current()`, events, and `Span::record()` are silently dropped
/// unless the dispatch is explicitly re-installed. This wrapper captures the
/// current dispatch and sets it as the thread-local default inside the closure.
pub(crate) fn traced_spawn_blocking<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let dispatch = tracing::dispatcher::get_default(|d| d.clone());
    #[allow(clippy::disallowed_methods)]
    tokio::task::spawn_blocking(move || {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        f()
    })
}

/// Git-backed repository for persisting and syncing memory files.
pub struct MemoryRepo {
    inner: Mutex<Repository>,
    root: PathBuf,
    reporter: SubsystemReporter,
    sync_reporter: SubsystemReporter,
}

// SAFETY: Repository holds raw pointers but is documented as safe to send
// across threads when not used concurrently. We guarantee exclusive access via
// the Mutex, so MemoryRepo is Send + Sync.
unsafe impl Send for MemoryRepo {}
unsafe impl Sync for MemoryRepo {}

impl MemoryRepo {
    /// Open an existing git repo at `path`, or initialise a new one.
    ///
    /// If `remote_url` is provided, ensures an `origin` remote exists pointing
    /// at that URL (creating or updating it as necessary).
    ///
    /// `reporter` receives `report_ok`/`report_err` after local git operations
    /// so `/readyz` reflects the repository's operational state passively.
    pub fn init_or_open(path: &Path, remote_url: Option<&str>) -> Result<Self, MemoryError> {
        Self::init_or_open_with_reporter(
            path,
            remote_url,
            SubsystemReporter::new(),
            SubsystemReporter::new(),
        )
    }

    /// Open or initialise a git repo with specific health reporters.
    ///
    /// `reporter` tracks local git operations; `sync_reporter` tracks push/pull.
    pub fn init_or_open_with_reporter(
        path: &Path,
        remote_url: Option<&str>,
        reporter: SubsystemReporter,
        sync_reporter: SubsystemReporter,
    ) -> Result<Self, MemoryError> {
        let _span = tracing::info_span!("repo.init").entered();

        let repo = if path.join(".git").exists() {
            Repository::open(path)?
        } else {
            let mut opts = git2::RepositoryInitOptions::new();
            opts.initial_head("main");
            let repo = Repository::init_opts(path, &opts)?;
            // Write a .gitignore so the vector index is never committed.
            let gitignore = path.join(".gitignore");
            if !gitignore.exists() {
                std::fs::write(&gitignore, ".memory-mcp-index/\n")?;
            }
            // Commit .gitignore as the initial commit.
            {
                let mut index = repo.index()?;
                index.add_path(Path::new(".gitignore"))?;
                index.write()?;
                let tree_oid = index.write_tree()?;
                let tree = repo.find_tree(tree_oid)?;
                let sig = Signature::now("memory-mcp", "memory-mcp@local")?;
                repo.commit(
                    Some("HEAD"),
                    &sig,
                    &sig,
                    "chore: init repository",
                    &tree,
                    &[],
                )?;
            }
            repo
        };

        // Set up or update the origin remote if a URL was provided.
        if let Some(url) = remote_url {
            match repo.find_remote("origin") {
                Ok(existing) => {
                    // Update the URL only when it differs from the current one.
                    let current_url = existing.url().unwrap_or("");
                    if current_url != url {
                        repo.remote_set_url("origin", url)?;
                        info!("updated origin remote URL to {}", redact_url(url));
                    }
                }
                Err(e) if e.code() == ErrorCode::NotFound => {
                    repo.remote("origin", url)?;
                    info!("created origin remote pointing at {}", redact_url(url));
                }
                Err(e) => return Err(MemoryError::Git(e)),
            }
        }

        Ok(Self {
            inner: Mutex::new(repo),
            root: path.to_path_buf(),
            reporter,
            sync_reporter,
        })
    }

    /// Root directory of the working tree.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Return the current HEAD commit SHA as a hex string, or `None` if the
    /// branch is unborn (no commits yet).
    pub async fn head_sha(self: &Arc<Self>) -> Option<String> {
        let me = Arc::clone(self);
        traced_spawn_blocking(move || {
            let repo = me.inner.lock().expect("repo mutex poisoned");
            let oid_bytes = capture_head_oid(&repo).ok()?;
            if oid_bytes == [0u8; 20] {
                return None;
            }
            git2::Oid::from_bytes(&oid_bytes)
                .ok()
                .map(|oid| oid.to_string())
        })
        .await
        .ok()
        .flatten()
    }

    /// Absolute path for a memory's markdown file inside the repo.
    fn memory_path(&self, name: &str, scope: &Scope) -> PathBuf {
        self.root
            .join(scope.dir_prefix().as_ref())
            .join(format!("{}.md", name))
    }

    /// Write the memory file to disk, then `git add` + `git commit`.
    ///
    /// All blocking work (mutex lock + fs ops + git2 ops) is performed inside
    /// `tokio::task::spawn_blocking` so the async executor is not stalled.
    pub async fn save_memory(self: &Arc<Self>, memory: &Memory) -> Result<(), MemoryError> {
        let file_path = self.memory_path(&memory.name, &memory.metadata.scope);
        self.assert_within_root(&file_path)?;

        let arc = Arc::clone(self);
        let memory = memory.clone();
        let name = memory.name.clone();

        let span = tracing::info_span!("repo.save", name = %name, oid = tracing::field::Empty);

        let result = traced_spawn_blocking(move || -> Result<(), MemoryError> {
            let _enter = span.entered();
            let repo = arc
                .inner
                .lock()
                .expect("lock poisoned — prior panic corrupted state");

            // Ensure the parent directory exists.
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let markdown = memory.to_markdown()?;
            arc.write_memory_file(&file_path, markdown.as_bytes())?;

            let mut index = arc.index_from_head(&repo)?;
            arc.stage_add(&mut index, &file_path)?;
            let oid = arc.commit_index(
                &repo,
                &mut index,
                &format!("chore: save memory '{}'", memory.name),
            )?;

            tracing::Span::current().record("oid", oid.to_string().as_str());
            info!(oid = %oid, "memory saved to repo");

            Ok(())
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("save_memory failed"),
        }
        result
    }

    /// Remove a memory's file and commit the deletion.
    pub async fn delete_memory(
        self: &Arc<Self>,
        name: &str,
        scope: &Scope,
    ) -> Result<(), MemoryError> {
        let file_path = self.memory_path(name, scope);
        self.assert_within_root(&file_path)?;

        let arc = Arc::clone(self);
        let name = name.to_string();
        let file_path_clone = file_path.clone();
        let span = tracing::info_span!("repo.delete", name = %name, oid = tracing::field::Empty);
        let result = traced_spawn_blocking(move || -> Result<(), MemoryError> {
            let _enter = span.entered();
            let repo = arc
                .inner
                .lock()
                .expect("lock poisoned — prior panic corrupted state");

            Self::assert_exists_no_symlink(&file_path_clone, &name)?;

            std::fs::remove_file(&file_path_clone)?;

            let mut index = arc.index_from_head(&repo)?;
            arc.stage_remove(&mut index, &file_path_clone)?;
            let oid = arc.commit_index(
                &repo,
                &mut index,
                &format!("chore: delete memory '{}'", name),
            )?;

            tracing::Span::current().record("oid", oid.to_string().as_str());
            info!(oid = %oid, "memory deleted from repo");

            Ok(())
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("delete_memory failed"),
        }
        result
    }

    /// Move a memory from one scope to another in a single git commit.
    ///
    /// Reads the source, builds a destination [`Memory`] with a new ID and
    /// the given name/scope (preserving content, tags, and source hint),
    /// writes the destination file, removes the source file, stages both
    /// changes, and commits atomically. If any step fails, the working
    /// tree is reset to HEAD via `checkout_head --force` so no dirty or
    /// untracked files are left behind.
    ///
    /// Returns the newly created destination [`Memory`] on success.
    pub async fn move_memory(
        self: &Arc<Self>,
        source_name: &str,
        source_scope: &Scope,
        dest_name: &MemoryName,
        dest_scope: &Scope,
    ) -> Result<Memory, MemoryError> {
        let source_path = self.memory_path(source_name, source_scope);
        self.assert_within_root(&source_path)?;

        let dest_path = self.memory_path(dest_name, dest_scope);
        self.assert_within_root(&dest_path)?;

        let arc = Arc::clone(self);
        let source_name = source_name.to_string();
        let dest_name = dest_name.clone();
        let dest_scope = dest_scope.clone();
        let span = tracing::info_span!(
            "repo.move",
            source_name = %source_name,
            dest_name = %dest_name,
            oid = tracing::field::Empty,
        );

        let result = traced_spawn_blocking(move || -> Result<Memory, MemoryError> {
            let _enter = span.entered();
            let repo = arc
                .inner
                .lock()
                .expect("lock poisoned — prior panic corrupted state");

            Self::assert_exists_no_symlink(&source_path, &source_name)?;

            // Read the source memory.
            let raw = arc.read_memory_file(&source_path)?;
            let source = Memory::from_markdown(&raw)?;

            // Build destination: new ID, same content/tags/source, new scope/name.
            let metadata = MemoryMetadata::new(
                dest_scope,
                source.metadata.tags.clone(),
                source.metadata.source.clone(),
            );
            let dest = Memory::from_validated(dest_name, source.content.clone(), metadata);

            // Write destination, delete source, stage both, commit.
            // If anything fails after we start modifying the working tree,
            // reset hard to HEAD so we never leave dirty/untracked files.
            let commit_result = (|| -> Result<git2::Oid, MemoryError> {
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let markdown = dest.to_markdown()?;
                arc.write_memory_file(&dest_path, markdown.as_bytes())?;
                std::fs::remove_file(&source_path)?;

                let mut index = arc.index_from_head(&repo)?;
                arc.stage_add(&mut index, &dest_path)?;
                arc.stage_remove(&mut index, &source_path)?;
                arc.commit_index(
                    &repo,
                    &mut index,
                    &format!("chore: move memory '{}' → '{}'", source_name, dest.name),
                )
            })();

            match commit_result {
                Ok(oid) => {
                    tracing::Span::current().record("oid", oid.to_string().as_str());
                    info!(oid = %oid, "memory moved in repo");
                    Ok(dest)
                }
                Err(e) => {
                    // Reset working tree + index to HEAD — restores source,
                    // removes destination, clears staged changes.
                    let mut checkout = git2::build::CheckoutBuilder::default();
                    checkout.force();
                    if let Err(reset_err) = repo.checkout_head(Some(&mut checkout)) {
                        warn!(
                            error = %reset_err,
                            "failed to reset working tree after move failure"
                        );
                    }
                    Err(e)
                }
            }
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("move_memory failed"),
        }
        result
    }

    /// Read and parse a memory from disk.
    pub async fn read_memory(
        self: &Arc<Self>,
        name: &str,
        scope: &Scope,
    ) -> Result<Memory, MemoryError> {
        let file_path = self.memory_path(name, scope);
        self.assert_within_root(&file_path)?;

        let arc = Arc::clone(self);
        let name = name.to_string();
        let span = tracing::debug_span!("repo.read", name = %name);
        let result = traced_spawn_blocking(move || -> Result<Memory, MemoryError> {
            let _enter = span.entered();
            Self::assert_exists_no_symlink(&file_path, &name)?;
            let raw = arc.read_memory_file(&file_path)?;
            Memory::from_markdown(&raw)
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(MemoryError::NotFound { .. }) | Err(MemoryError::InvalidInput { .. }) => {
                // Application-level errors — the repo itself is fine.
                self.reporter.report_ok();
            }
            Err(_) => self.reporter.report_err("read_memory failed"),
        }
        result
    }

    /// List all memories, optionally filtered by scope.
    pub async fn list_memories(
        self: &Arc<Self>,
        scope: Option<&Scope>,
    ) -> Result<Vec<Memory>, MemoryError> {
        let root = self.root.clone();
        let scope_clone = scope.cloned();
        let span = tracing::debug_span!("repo.list", file_count = tracing::field::Empty,);

        let result = traced_spawn_blocking(move || -> Result<Vec<Memory>, MemoryError> {
            let _enter = span.entered();
            let dirs: Vec<PathBuf> = match scope_clone.as_ref() {
                Some(s) => vec![root.join(s.dir_prefix().as_ref())],
                None => {
                    // Walk both global/ and projects/* (on-disk namespace directories)
                    let mut dirs = Vec::new();
                    let global = root.join("global");
                    if global.exists() {
                        dirs.push(global);
                    }
                    let projects = root.join("projects");
                    if projects.exists() {
                        for entry in std::fs::read_dir(&projects)? {
                            let entry = entry?;
                            if entry.file_type()?.is_dir() {
                                dirs.push(entry.path());
                            }
                        }
                    }
                    dirs
                }
            };

            fn collect_md_files(dir: &Path, out: &mut Vec<Memory>) -> Result<(), MemoryError> {
                if !dir.exists() {
                    return Ok(());
                }
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    let ft = entry.file_type()?;
                    // Skip symlinks entirely to prevent directory traversal.
                    if ft.is_symlink() {
                        warn!(
                            "skipping symlink at {:?} — symlinks are not permitted in the memory store",
                            path
                        );
                        continue;
                    }
                    if ft.is_dir() {
                        collect_md_files(&path, out)?;
                    } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                        let raw = std::fs::read_to_string(&path)?;
                        match Memory::from_markdown(&raw) {
                            Ok(m) => out.push(m),
                            Err(e) => {
                                warn!("skipping {:?}: {}", path, e);
                            }
                        }
                    }
                }
                Ok(())
            }

            let mut memories = Vec::new();
            for dir in dirs {
                collect_md_files(&dir, &mut memories)?;
            }

            tracing::Span::current().record("file_count", memories.len());

            Ok(memories)
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("list_memories failed"),
        }
        result
    }

    /// Push local commits to `origin/<branch>`.
    ///
    /// If no `origin` remote is configured the call is a no-op (local-only
    /// mode). Auth failures are propagated as `MemoryError::Auth`.
    pub async fn push(
        self: &Arc<Self>,
        auth: &AuthProvider,
        branch: &str,
    ) -> Result<(), MemoryError> {
        // Resolve the token early so we can move it (Send) into the
        // spawn_blocking closure. We defer failing until after we've confirmed
        // that origin exists — local-only mode needs no token at all.
        let token_result = auth.resolve_token();
        let arc = Arc::clone(self);
        let branch = branch.to_string();
        let span = tracing::debug_span!("repo.push", branch = %branch);

        let result = traced_spawn_blocking(move || -> Result<(), MemoryError> {
            let _enter = span.entered();
            let repo = arc
                .inner
                .lock()
                .expect("lock poisoned — prior panic corrupted state");

            let mut remote = match repo.find_remote("origin") {
                Ok(r) => r,
                Err(e) if e.code() == ErrorCode::NotFound => {
                    warn!("push: no origin remote configured — skipping (local-only mode)");
                    return Ok(());
                }
                Err(e) => return Err(MemoryError::Git(e)),
            };

            // Origin exists — we need the token now.
            let token = token_result?;
            let mut callbacks = build_auth_callbacks(token);

            // git2's Remote::push() does not surface server-side rejections
            // through its return value — they arrive via this callback.
            let rejections: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let rej = Arc::clone(&rejections);
            callbacks.push_update_reference(move |refname, status| {
                if let Some(msg) = status {
                    rej.lock()
                        .expect("rejection lock poisoned")
                        .push(format!("{refname}: {msg}"));
                }
                Ok(())
            });

            let mut push_opts = git2::PushOptions::new();
            push_opts.remote_callbacks(callbacks);

            let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
            if let Err(e) = remote.push(&[&refspec], Some(&mut push_opts)) {
                warn!("push to origin failed at transport level: {e}");
                return Err(MemoryError::Git(e));
            }

            let rejected = rejections.lock().expect("rejection lock poisoned");
            if !rejected.is_empty() {
                return Err(MemoryError::PushRejected(rejected.join("; ")));
            }

            info!("pushed branch '{}' to origin", branch);
            Ok(())
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.sync_reporter.report_ok(),
            Err(_) => self.sync_reporter.report_err("push failed"),
        }
        result
    }

    /// Perform a normal (non-fast-forward) merge of `fetch_commit` into HEAD.
    ///
    /// Resolves any conflicts using recency-based auto-resolution, creates the
    /// merge commit, and cleans up MERGE state.
    fn merge_with_remote(
        &self,
        repo: &git2::Repository,
        fetch_commit: &git2::AnnotatedCommit,
        branch: &str,
    ) -> Result<PullResult, MemoryError> {
        // Capture old HEAD before the merge commit.
        // HEAD must exist here — merge analysis would not reach this path
        // with an unborn branch. Propagate the error if it somehow does.
        let oid = repo.head()?.peel_to_commit()?.id();
        let mut old_head = [0u8; 20];
        old_head.copy_from_slice(oid.as_bytes());

        let mut merge_opts = MergeOptions::new();
        merge_opts.fail_on_conflict(false);
        repo.merge(&[fetch_commit], Some(&mut merge_opts), None)?;

        let mut index = repo.index()?;
        let conflicts_resolved = if index.has_conflicts() {
            self.resolve_conflicts_by_recency(repo, &mut index)?
        } else {
            0
        };

        // Safety check: if any conflicts remain after auto-resolution,
        // clean up the MERGE state and surface a clear error rather than
        // letting write_tree() fail with an opaque message.
        if index.has_conflicts() {
            let _ = repo.cleanup_state();
            return Err(MemoryError::Internal(
                "unresolved conflicts remain after auto-resolution".into(),
            ));
        }

        // Write the merged tree and create the merge commit.
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;
        let sig = self.signature(repo)?;

        let head_commit = repo.head()?.peel_to_commit()?;
        let fetch_commit_obj = repo.find_commit(fetch_commit.id())?;

        let new_commit_oid = repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &format!("chore: merge origin/{}", branch),
            &tree,
            &[&head_commit, &fetch_commit_obj],
        )?;

        repo.cleanup_state()?;

        let mut new_head = [0u8; 20];
        new_head.copy_from_slice(new_commit_oid.as_bytes());

        info!(
            "pull: merge complete ({} conflicts auto-resolved)",
            conflicts_resolved
        );
        Ok(PullResult::Merged {
            conflicts_resolved,
            old_head,
            new_head,
        })
    }

    /// Pull from `origin/<branch>` and merge into the current HEAD.
    ///
    /// Uses a recency-based auto-resolution strategy for conflicts: the version
    /// with the more recent `updated_at` frontmatter timestamp wins. If
    /// timestamps are equal or unparseable, the local version is kept.
    pub async fn pull(
        self: &Arc<Self>,
        auth: &AuthProvider,
        branch: &str,
    ) -> Result<PullResult, MemoryError> {
        // Resolve the token early so we can move it (Send) into the
        // spawn_blocking closure. We defer failing until after we've confirmed
        // that origin exists — local-only mode needs no token at all.
        let token_result = auth.resolve_token();
        let arc = Arc::clone(self);
        let branch = branch.to_string();
        let span = tracing::debug_span!("repo.pull", branch = %branch);

        let result = traced_spawn_blocking(move || -> Result<PullResult, MemoryError> {
            let _enter = span.entered();
            let repo = arc
                .inner
                .lock()
                .expect("lock poisoned — prior panic corrupted state");

            // ---- 1. Find origin -------------------------------------------------
            let mut remote = match repo.find_remote("origin") {
                Ok(r) => r,
                Err(e) if e.code() == ErrorCode::NotFound => {
                    warn!("pull: no origin remote configured — skipping (local-only mode)");
                    return Ok(PullResult::NoRemote);
                }
                Err(e) => return Err(MemoryError::Git(e)),
            };

            // Origin exists — we need the token now.
            let token = token_result?;

            // ---- 2. Fetch -------------------------------------------------------
            let callbacks = build_auth_callbacks(token);
            let mut fetch_opts = git2::FetchOptions::new();
            fetch_opts.remote_callbacks(callbacks);
            remote.fetch(&[&branch], Some(&mut fetch_opts), None)?;

            // ---- 3. Resolve FETCH_HEAD ------------------------------------------
            let fetch_head = match repo.find_reference("FETCH_HEAD") {
                Ok(r) => r,
                Err(e) if e.code() == ErrorCode::NotFound => {
                    // Empty remote — nothing to merge.
                    return Ok(PullResult::UpToDate);
                }
                Err(e)
                    if e.class() == git2::ErrorClass::Reference
                        && e.message().contains("corrupted") =>
                {
                    // Empty/corrupted FETCH_HEAD (e.g. remote has no commits yet).
                    info!("pull: FETCH_HEAD is empty or corrupted — treating as empty remote");
                    return Ok(PullResult::UpToDate);
                }
                Err(e) => return Err(MemoryError::Git(e)),
            };
            let fetch_commit = match repo.reference_to_annotated_commit(&fetch_head) {
                Ok(c) => c,
                Err(e) if e.class() == git2::ErrorClass::Reference => {
                    // FETCH_HEAD exists but can't be resolved (empty remote).
                    info!("pull: FETCH_HEAD not resolvable — treating as empty remote");
                    return Ok(PullResult::UpToDate);
                }
                Err(e) => return Err(MemoryError::Git(e)),
            };

            // ---- 4. Merge analysis ----------------------------------------------
            let (analysis, _preference) = repo.merge_analysis(&[&fetch_commit])?;

            if analysis.is_up_to_date() {
                info!("pull: already up to date");
                return Ok(PullResult::UpToDate);
            }

            if analysis.is_fast_forward() {
                return fast_forward(&repo, &fetch_commit, &branch);
            }

            arc.merge_with_remote(&repo, &fetch_commit, &branch)
        })
        .await
        .map_err(|e| MemoryError::Join(e.to_string()))?;

        match &result {
            Ok(_) => self.sync_reporter.report_ok(),
            Err(_) => self.sync_reporter.report_err("pull failed"),
        }
        result
    }

    /// Diff two commits and return the memory files that changed.
    ///
    /// Only `.md` files under `global/` or `projects/` (namespace directories) are considered.
    /// Added/modified files go into `upserted`; deleted files go into `removed`.
    /// Qualified names are returned without the `.md` suffix (e.g. `"global/foo"`).
    ///
    /// This is the published, stable change-set surface: it derives its strings
    /// straight from the git delta paths and reports **every** changed `.md`
    /// file — including ones whose blob is non-UTF-8 or has unparseable
    /// frontmatter — because a path is always available even when the content
    /// is not. This is deliberately distinct from the crate-internal
    /// `Self::diff_changed_refs`, which resolves each file to a canonical
    /// [`crate::types::MemoryRef`] (and drops/counts the unresolvable ones) for
    /// the complete-or-degraded index mirror. The public method exists to
    /// preserve the exact 0.16.0 value contract; the mirror path uses
    /// `diff_changed_refs`.
    ///
    /// Must be called from within `spawn_blocking` since it uses git2.
    pub fn diff_changed_memories(
        &self,
        old_oid: [u8; 20],
        new_oid: [u8; 20],
    ) -> Result<ChangedMemories, MemoryError> {
        let repo = self
            .inner
            .lock()
            .expect("lock poisoned — prior panic corrupted state");

        let new_git_oid = git2::Oid::from_bytes(&new_oid).map_err(MemoryError::Git)?;
        let new_tree = repo.find_commit(new_git_oid)?.tree()?;

        // A zero OID indicates an unborn branch (no prior commits). In that case,
        // diff against an empty tree so all files appear as additions.
        let diff = if old_oid == [0u8; 20] {
            repo.diff_tree_to_tree(None, Some(&new_tree), None)?
        } else {
            let old_git_oid = git2::Oid::from_bytes(&old_oid).map_err(MemoryError::Git)?;
            let old_tree = repo.find_commit(old_git_oid)?.tree()?;
            repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)?
        };

        let mut changes = ChangedMemories::default();

        diff.foreach(
            &mut |delta, _progress| {
                use git2::Delta;

                let path = match delta.new_file().path().or_else(|| delta.old_file().path()) {
                    Some(p) => p,
                    None => return true,
                };

                let path_str = match path.to_str() {
                    Some(s) => s,
                    None => return true,
                };

                // Only care about .md files under global/ or projects/ (namespace directories)
                if !path_str.ends_with(".md") {
                    return true;
                }
                if !path_str.starts_with("global/") && !path_str.starts_with("projects/") {
                    return true;
                }

                // Strip the .md suffix to get the qualified name.
                let qualified = &path_str[..path_str.len() - 3];

                match delta.status() {
                    Delta::Added | Delta::Modified => {
                        changes.upserted.push(qualified.to_string());
                    }
                    Delta::Renamed | Delta::Copied => {
                        // For renames, the old path must be removed from the index
                        // to avoid leaving a ghost vector behind.
                        if matches!(delta.status(), Delta::Renamed) {
                            if let Some(old_path) = delta.old_file().path().and_then(|p| p.to_str())
                            {
                                if old_path.ends_with(".md")
                                    && (old_path.starts_with("global/")
                                        || old_path.starts_with("projects/"))
                                {
                                    changes
                                        .removed
                                        .push(old_path[..old_path.len() - 3].to_string());
                                }
                            }
                        }
                        changes.upserted.push(qualified.to_string());
                    }
                    Delta::Deleted => {
                        changes.removed.push(qualified.to_string());
                    }
                    _ => {}
                }

                true
            },
            None,
            None,
            None,
        )
        .map_err(MemoryError::Git)?;

        Ok(changes)
    }

    /// Diff two commits and resolve the changed memory files to structured refs.
    ///
    /// Only `.md` files under `global/` or `projects/` (namespace directories) are considered.
    /// Added/modified files go into `upserted`; deleted files go into `removed`.
    /// A git type change (e.g. a tracked regular memory file replaced by a
    /// symlink, or vice versa) is treated as an old-side removal plus a
    /// new-side upsert, so a memory that `list_memories` will no longer see
    /// (symlinks are skipped there) is dropped from derived indexes rather
    /// than left stale.
    ///
    /// Each changed file is resolved to a [`crate::types::MemoryRef`] by
    /// parsing the blob's YAML frontmatter (new tree for upserts, old tree
    /// for removals) — the same authority `list_memories` uses to build the
    /// canonical index keys, so hierarchical scope paths are never split
    /// ambiguously. Files whose frontmatter cannot be parsed, or whose object
    /// is not a memory at all (e.g. a symlink blob), are counted in
    /// [`ResolvedChanges::unresolved`] rather than silently dropped; a git
    /// failure while reading a blob the diff itself reported is an error.
    ///
    /// Must be called from within `spawn_blocking` since it uses git2.
    pub(crate) fn diff_changed_refs(
        &self,
        old_oid: [u8; 20],
        new_oid: [u8; 20],
    ) -> Result<ResolvedChanges, MemoryError> {
        let repo = self
            .inner
            .lock()
            .expect("lock poisoned — prior panic corrupted state");

        let new_git_oid = git2::Oid::from_bytes(&new_oid).map_err(MemoryError::Git)?;
        let new_tree = repo.find_commit(new_git_oid)?.tree()?;

        // A zero OID indicates an unborn branch (no prior commits). In that case,
        // diff against an empty tree so all files appear as additions.
        let old_tree = if old_oid == [0u8; 20] {
            None
        } else {
            let old_git_oid = git2::Oid::from_bytes(&old_oid).map_err(MemoryError::Git)?;
            Some(repo.find_commit(old_git_oid)?.tree()?)
        };
        let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;

        /// A changed memory file path, tagged with the tree that holds its blob.
        enum ChangedPath {
            Upserted(String),
            Removed(String),
        }

        fn tracked_memory_path(path: Option<&std::path::Path>) -> Option<&str> {
            let path_str = path?.to_str()?;
            // Only care about .md files under global/ or projects/ (namespace directories)
            if !path_str.ends_with(".md") {
                return None;
            }
            if !path_str.starts_with("global/") && !path_str.starts_with("projects/") {
                return None;
            }
            Some(path_str)
        }

        // Collect the changed paths first: `foreach` callbacks cannot
        // propagate errors, and blob resolution must be able to fail.
        let mut changed: Vec<ChangedPath> = Vec::new();
        diff.foreach(
            &mut |delta, _progress| {
                use git2::Delta;

                match delta.status() {
                    Delta::Added | Delta::Modified => {
                        if let Some(p) = tracked_memory_path(delta.new_file().path()) {
                            changed.push(ChangedPath::Upserted(p.to_string()));
                        }
                    }
                    Delta::Renamed | Delta::Copied => {
                        // For renames, the old path must be removed from the index
                        // to avoid leaving a ghost vector behind.
                        if matches!(delta.status(), Delta::Renamed) {
                            if let Some(p) = tracked_memory_path(delta.old_file().path()) {
                                changed.push(ChangedPath::Removed(p.to_string()));
                            }
                        }
                        if let Some(p) = tracked_memory_path(delta.new_file().path()) {
                            changed.push(ChangedPath::Upserted(p.to_string()));
                        }
                    }
                    Delta::Deleted => {
                        if let Some(p) = tracked_memory_path(delta.old_file().path()) {
                            changed.push(ChangedPath::Removed(p.to_string()));
                        }
                    }
                    Delta::Typechange => {
                        // The tracked path changed object type (e.g. a regular
                        // memory file `100644` became a symlink `120000`, or the
                        // reverse). Mirror it as remove-old + add-new: the old
                        // blob must leave the index, and the new object is
                        // upserted only if it resolves to a valid memory. When
                        // the new side is a symlink (its blob is a target path,
                        // not memory markdown) resolution fails and it is
                        // counted `unresolved`, forcing degrade + repair — which
                        // matches `list_memories`, that skips symlinks entirely.
                        if let Some(p) = tracked_memory_path(delta.old_file().path()) {
                            changed.push(ChangedPath::Removed(p.to_string()));
                        }
                        if let Some(p) = tracked_memory_path(delta.new_file().path()) {
                            changed.push(ChangedPath::Upserted(p.to_string()));
                        }
                    }
                    _ => {}
                }

                true
            },
            None,
            None,
            None,
        )
        .map_err(MemoryError::Git)?;

        /// Resolve a changed path to a `MemoryRef` via the blob's frontmatter.
        ///
        /// `Ok(None)` means the blob exists but is not a parseable memory
        /// (the caller counts it as unresolved); `Err` means git could not
        /// produce a blob the diff itself reported, which indicates a
        /// repository-level problem rather than a bad file.
        fn resolve_blob_ref(
            repo: &Repository,
            tree: &git2::Tree<'_>,
            path: &str,
        ) -> Result<Option<crate::types::MemoryRef>, MemoryError> {
            let entry = tree.get_path(std::path::Path::new(path))?;
            let blob = entry.to_object(repo)?.peel_to_blob()?;
            let Ok(raw) = std::str::from_utf8(blob.content()) else {
                warn!(
                    path,
                    "changed memory file is not UTF-8; cannot resolve its reference"
                );
                return Ok(None);
            };
            match Memory::from_markdown(raw) {
                Ok(memory) => Ok(Some(memory.mem_ref())),
                Err(e) => {
                    warn!(
                        path,
                        error = %e,
                        "changed memory file has unparseable frontmatter; cannot resolve its reference"
                    );
                    Ok(None)
                }
            }
        }

        let mut changes = ResolvedChanges::default();
        for change in changed {
            match change {
                ChangedPath::Upserted(path) => match resolve_blob_ref(&repo, &new_tree, &path)? {
                    Some(mref) => changes.upserted.push(mref),
                    None => changes.unresolved += 1,
                },
                ChangedPath::Removed(path) => {
                    // Removals can only appear when an old tree exists.
                    let Some(old_tree) = old_tree.as_ref() else {
                        return Err(MemoryError::Index(format!(
                            "diff reported removal of '{path}' without an old tree"
                        )));
                    };
                    match resolve_blob_ref(&repo, old_tree, &path)? {
                        Some(mref) => changes.removed.push(mref),
                        None => changes.unresolved += 1,
                    }
                }
            }
        }

        Ok(changes)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Resolve all index conflicts using a recency-based strategy.
    ///
    /// For each conflicted entry, the version with the more recent `updated_at`
    /// frontmatter timestamp wins. Ties and parse failures fall back to "ours"
    /// (local). Returns the number of files resolved.
    fn resolve_conflicts_by_recency(
        &self,
        repo: &Repository,
        index: &mut git2::Index,
    ) -> Result<usize, MemoryError> {
        // Collect conflict info first to avoid borrow issues with the index.
        struct ConflictInfo {
            path: PathBuf,
            our_blob: Option<Vec<u8>>,
            their_blob: Option<Vec<u8>>,
        }

        let mut conflicts_info: Vec<ConflictInfo> = Vec::new();

        {
            let conflicts = index.conflicts()?;
            for conflict in conflicts {
                let conflict = conflict?;

                let path = conflict
                    .our
                    .as_ref()
                    .or(conflict.their.as_ref())
                    .and_then(|e| std::str::from_utf8(&e.path).ok())
                    .map(|s| self.root.join(s));

                let path = match path {
                    Some(p) => p,
                    None => continue,
                };

                let our_blob = conflict
                    .our
                    .as_ref()
                    .and_then(|e| repo.find_blob(e.id).ok())
                    .map(|b| b.content().to_vec());

                let their_blob = conflict
                    .their
                    .as_ref()
                    .and_then(|e| repo.find_blob(e.id).ok())
                    .map(|b| b.content().to_vec());

                conflicts_info.push(ConflictInfo {
                    path,
                    our_blob,
                    their_blob,
                });
            }
        }

        let mut resolved = 0usize;

        for info in conflicts_info {
            let our_str = info
                .our_blob
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(str::to_owned);
            let their_str = info
                .their_blob
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(str::to_owned);

            let our_ts = our_str
                .as_deref()
                .and_then(|s| Memory::from_markdown(s).ok())
                .map(|m| m.metadata.updated_at);
            let their_ts = their_str
                .as_deref()
                .and_then(|s| Memory::from_markdown(s).ok())
                .map(|m| m.metadata.updated_at);

            // Pick the winning content as raw bytes.
            let (chosen_bytes, label): (Vec<u8>, String) =
                match (our_str.as_deref(), their_str.as_deref()) {
                    (Some(ours), Some(theirs)) => match (our_ts, their_ts) {
                        (Some(ot), Some(tt)) if tt > ot => (
                            theirs.as_bytes().to_vec(),
                            format!("theirs (updated_at: {})", tt),
                        ),
                        (Some(ot), _) => (
                            ours.as_bytes().to_vec(),
                            format!("ours (updated_at: {})", ot),
                        ),
                        _ => (
                            ours.as_bytes().to_vec(),
                            "ours (timestamp unparseable)".to_string(),
                        ),
                    },
                    (Some(ours), None) => (
                        ours.as_bytes().to_vec(),
                        "ours (theirs missing)".to_string(),
                    ),
                    (None, Some(theirs)) => (
                        theirs.as_bytes().to_vec(),
                        "theirs (ours missing)".to_string(),
                    ),
                    (None, None) => {
                        // Both UTF-8 conversions failed — fall back to raw blob bytes.
                        match (info.our_blob.as_deref(), info.their_blob.as_deref()) {
                            (Some(ours), _) => {
                                (ours.to_vec(), "ours (binary/non-UTF-8)".to_string())
                            }
                            (_, Some(theirs)) => {
                                (theirs.to_vec(), "theirs (binary/non-UTF-8)".to_string())
                            }
                            (None, None) => {
                                // Both blobs truly absent — remove the entry from
                                // the index so write_tree() succeeds.
                                warn!(
                                    "conflict at '{}': both sides missing — removing from index",
                                    info.path.display()
                                );
                                let relative = info.path.strip_prefix(&self.root).map_err(|e| {
                                    MemoryError::InvalidInput {
                                        reason: format!(
                                            "path strip error during conflict resolution: {}",
                                            e
                                        ),
                                    }
                                })?;
                                index.conflict_remove(relative)?;
                                resolved += 1;
                                continue;
                            }
                        }
                    }
                };

            warn!(
                "conflict resolved: {} — kept {}",
                info.path.display(),
                label
            );

            // Write the chosen content to the working directory — going through
            // assert_within_root and write_memory_file enforces path-traversal
            // and symlink protections.
            self.assert_within_root(&info.path)?;
            if let Some(parent) = info.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.write_memory_file(&info.path, &chosen_bytes)?;

            // Stage the resolution.
            let relative =
                info.path
                    .strip_prefix(&self.root)
                    .map_err(|e| MemoryError::InvalidInput {
                        reason: format!("path strip error during conflict resolution: {}", e),
                    })?;
            index.add_path(relative)?;

            resolved += 1;
        }

        Ok(resolved)
    }

    fn signature<'r>(&self, repo: &'r Repository) -> Result<Signature<'r>, MemoryError> {
        // Try repo config first, then fall back to a default.
        let sig = repo
            .signature()
            .or_else(|_| Signature::now("memory-mcp", "memory-mcp@local"))?;
        Ok(sig)
    }

    /// Return an index whose contents match the repository's current HEAD tree.
    ///
    /// `MemoryRepo` holds one `Repository` for the process lifetime, and
    /// libgit2 caches the index it hands back. Nothing re-reads it, so a
    /// commit made outside pali leaves the cached index describing a tree
    /// that predates that commit. Committing from the stale index writes it
    /// back over `.git/index` and builds a tree from it, producing a commit
    /// that presents as a wholesale revert of the external writer's work with
    /// one file added on top. The working tree stays correct throughout,
    /// which is why it can go unnoticed for weeks — only HEAD is wrong.
    ///
    /// Basing the index on HEAD instead means a pali commit always says
    /// "HEAD, plus exactly what I staged," no matter who else has written.
    ///
    /// This is deliberately *not* fail-closed on divergence: a memory store
    /// must not answer confusion by declining to remember.
    ///
    /// Only the index is reset — the working tree is untouched, so staging a
    /// path afterwards still picks up the file on disk. One consequence:
    /// anything an outside writer has staged but not yet committed is
    /// unstaged by the next pali write (the file stays on disk, and it was
    /// also lost by the cached-index code before #365).
    fn index_from_head(&self, repo: &Repository) -> Result<git2::Index, MemoryError> {
        let mut index = repo.index()?;
        match repo.head() {
            Ok(head) => {
                let tree = head.peel_to_tree()?;
                index.read_tree(&tree)?;
            }
            Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => {
                // No tree to read. `init_or_open` always makes a first
                // commit, so the reachable case is a branch ref removed
                // from outside, not a fresh repo. Clearing here would make
                // the next commit a root holding one file. Fall back to the
                // freshest record of what is tracked, the on-disk index,
                // so this path never remembers less than it did before #365.
                index.read(true)?;
            }
            Err(e) => return Err(MemoryError::Git(e)),
        }
        Ok(index)
    }

    /// Commit the current index state and return the new commit's `Oid`.
    fn commit_index(
        &self,
        repo: &Repository,
        index: &mut git2::Index,
        message: &str,
    ) -> Result<git2::Oid, MemoryError> {
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;
        let sig = self.signature(repo)?;

        let oid = match repo.head() {
            Ok(head) => {
                let parent_commit = head.peel_to_commit()?;
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent_commit])?
            }
            Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => {
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?
            }
            Err(e) => return Err(MemoryError::Git(e)),
        };

        Ok(oid)
    }

    /// Stage a file addition in the given index.
    fn stage_add(&self, index: &mut git2::Index, file_path: &Path) -> Result<(), MemoryError> {
        let relative =
            file_path
                .strip_prefix(&self.root)
                .map_err(|e| MemoryError::InvalidInput {
                    reason: format!("path strip error: {}", e),
                })?;
        index.add_path(relative)?;
        Ok(())
    }

    /// Stage a file removal in the given index.
    fn stage_remove(&self, index: &mut git2::Index, file_path: &Path) -> Result<(), MemoryError> {
        let relative =
            file_path
                .strip_prefix(&self.root)
                .map_err(|e| MemoryError::InvalidInput {
                    reason: format!("path strip error: {}", e),
                })?;
        index.remove_path(relative)?;
        Ok(())
    }

    /// Assert that `path` exists on disk and is not a symlink.
    ///
    /// Returns `NotFound` if the file is absent, `InvalidInput` if it is a
    /// symlink. This is the standard pre-check for operations that read or
    /// remove an existing memory file.
    fn assert_exists_no_symlink(path: &Path, name: &str) -> Result<(), MemoryError> {
        match std::fs::symlink_metadata(path) {
            Err(_) => Err(MemoryError::NotFound {
                name: name.to_string(),
            }),
            Ok(m) if m.file_type().is_symlink() => Err(MemoryError::InvalidInput {
                reason: format!(
                    "path '{}' is a symlink, which is not permitted",
                    path.display()
                ),
            }),
            Ok(_) => Ok(()),
        }
    }

    /// Assert that `path` remains under `self.root` after canonicalisation,
    /// preventing path-traversal attacks.
    fn assert_within_root(&self, path: &Path) -> Result<(), MemoryError> {
        // The file may not exist yet, so we canonicalize its parent and
        // then re-append the filename.
        let parent = path.parent().unwrap_or(path);
        let filename = path.file_name().ok_or_else(|| MemoryError::InvalidInput {
            reason: "path has no filename component".to_string(),
        })?;

        // If the parent doesn't exist yet we check as many ancestors as
        // necessary until we find one that does.
        let canon_parent = {
            let mut p = parent.to_path_buf();
            let mut suffixes: Vec<std::ffi::OsString> = Vec::new();
            loop {
                match p.canonicalize() {
                    Ok(c) => {
                        let mut full = c;
                        for s in suffixes.into_iter().rev() {
                            full.push(s);
                        }
                        break full;
                    }
                    Err(_) => {
                        if let Some(name) = p.file_name() {
                            suffixes.push(name.to_os_string());
                        }
                        match p.parent() {
                            Some(par) => p = par.to_path_buf(),
                            None => {
                                return Err(MemoryError::InvalidInput {
                                    reason: "cannot resolve any ancestor of path".into(),
                                });
                            }
                        }
                    }
                }
            }
        };

        let resolved = canon_parent.join(filename);

        let canon_root = self
            .root
            .canonicalize()
            .map_err(|e| MemoryError::InvalidInput {
                reason: format!("cannot canonicalize repo root: {}", e),
            })?;

        if !resolved.starts_with(&canon_root) {
            return Err(MemoryError::InvalidInput {
                reason: format!(
                    "path '{}' escapes repository root '{}'",
                    resolved.display(),
                    canon_root.display()
                ),
            });
        }

        // Reject any symlinks within the repo root. We check each existing
        // component of `resolved` that lies inside `canon_root` — if any is a
        // symlink the request is rejected, because canonicalization already
        // followed it and the prefix check above would silently pass.
        {
            let mut probe = canon_root.clone();
            // Collect the path components that are beneath the root.
            let relative =
                resolved
                    .strip_prefix(&canon_root)
                    .map_err(|e| MemoryError::InvalidInput {
                        reason: format!("path strip error: {}", e),
                    })?;
            for component in relative.components() {
                probe.push(component);
                // Only check components that currently exist on disk.
                if (probe.exists() || probe.symlink_metadata().is_ok())
                    && probe
                        .symlink_metadata()
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                {
                    return Err(MemoryError::InvalidInput {
                        reason: format!(
                            "path component '{}' is a symlink, which is not allowed",
                            probe.display()
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    /// Atomically write `data` to `path` via temp-file + rename.
    ///
    /// Defense-in-depth against symlink attacks (layered):
    /// 1. `validate_path` rejects symlinks in all path components.
    /// 2. An `lstat` check here catches symlinks created between
    ///    validation and write (narrows the TOCTOU window).
    /// 3. On Unix, an `O_NOFOLLOW` probe on the final path detects
    ///    symlinks planted in the window between lstat and
    ///    `atomic_write`. The temp file itself is separately guarded
    ///    by `O_NOFOLLOW` inside `write_tmp`.
    fn write_memory_file(&self, path: &Path, data: &[u8]) -> Result<(), MemoryError> {
        // Layer 2: lstat — reject if the target is currently a symlink.
        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(MemoryError::InvalidInput {
                reason: format!("refusing to write through symlink: {}", path.display()),
            });
        }

        // Layer 3 (Unix): O_NOFOLLOW probe — kernel-level symlink rejection.
        // NotFound is fine (file doesn't exist yet); any other error (ELOOP
        // from a symlink, permission denied, etc.) is rejected.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            if let Err(e) = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                // NotFound is fine — the file doesn't exist yet.
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(MemoryError::InvalidInput {
                        reason: format!("O_NOFOLLOW check failed for {}: {e}", path.display()),
                    });
                }
            }
        }

        crate::fs_util::atomic_write(path, data)?;
        Ok(())
    }

    /// Open `path` for reading using `O_NOFOLLOW` on Unix, then return its
    /// contents as a `String`.
    ///
    /// On non-Unix platforms falls back to `std::fs::read_to_string`.
    fn read_memory_file(&self, path: &Path) -> Result<String, MemoryError> {
        #[cfg(unix)]
        {
            use std::io::Read as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            Ok(buf)
        }
        #[cfg(not(unix))]
        {
            Ok(std::fs::read_to_string(path)?)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthProvider;
    use crate::types::{Memory, MemoryMetadata, PullResult, Scope, ScopePath};
    use std::sync::Arc;

    fn test_auth() -> AuthProvider {
        AuthProvider::with_token("test-token-unused-for-file-remotes")
    }

    fn make_memory(name: &str, content: &str, updated_at_secs: i64) -> Memory {
        let meta = MemoryMetadata {
            tags: vec![],
            scope: Scope::Root,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(updated_at_secs, 0).unwrap(),
            source: None,
        };
        Memory::new(name, content, meta).unwrap()
    }

    fn setup_bare_remote() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        git2::Repository::init_bare(dir.path()).expect("failed to init bare repo");
        let url = format!("file://{}", dir.path().display());
        (dir, url)
    }

    fn open_repo(dir: &tempfile::TempDir, remote_url: Option<&str>) -> Arc<MemoryRepo> {
        Arc::new(MemoryRepo::init_or_open(dir.path(), remote_url).expect("failed to init repo"))
    }

    // -- #365: cached index must not revert an external writer -------------

    /// Commit a file directly with git2, bypassing `MemoryRepo` entirely —
    /// the way a human running `git commit` in the memories checkout does.
    fn commit_out_of_band(dir: &tempfile::TempDir, filename: &str, body: &str) {
        let repo = git2::Repository::open(dir.path()).expect("open for out-of-band write");
        std::fs::write(dir.path().join(filename), body).expect("write out-of-band file");

        let mut index = repo.index().expect("index");
        index
            .add_path(std::path::Path::new(filename))
            .expect("stage out-of-band file");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write_tree");
        let tree = repo.find_tree(tree_oid).expect("find_tree");
        let sig = git2::Signature::now("outsider", "outsider@example.invalid").unwrap();
        let parent = repo
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("peel to commit");
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "out-of-band commit",
            &tree,
            &[&parent],
        )
        .expect("commit out-of-band");
    }

    /// True if `filename` is present in the repository's current HEAD tree.
    fn in_head_tree(dir: &tempfile::TempDir, filename: &str) -> bool {
        let repo = git2::Repository::open(dir.path()).expect("open for tree check");
        let head = repo.head().expect("head");
        let tree = head.peel_to_tree().expect("peel to tree");
        let found = tree.get_name(filename).is_some();
        drop(tree);
        found
    }

    #[tokio::test]
    async fn save_after_external_commit_does_not_revert_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        // A save through the API, so the cached index is populated and warm.
        repo.save_memory(&make_memory("first", "one", 1_700_000_100))
            .await
            .expect("first save");

        // Someone else commits, without going through pali.
        commit_out_of_band(&dir, "outsider.md", "not pali's file\n");
        assert!(in_head_tree(&dir, "outsider.md"), "precondition");

        // A second save must add its file WITHOUT deleting the outsider's.
        repo.save_memory(&make_memory("second", "two", 1_700_000_200))
            .await
            .expect("second save");

        assert!(
            in_head_tree(&dir, "outsider.md"),
            "#365: save_memory built its tree from a stale cached index and \
             reverted a commit made outside pali"
        );
        repo.read_memory("first", &Scope::Root)
            .await
            .expect("earlier memory still present");
        repo.read_memory("second", &Scope::Root)
            .await
            .expect("new memory written");
    }

    #[tokio::test]
    async fn delete_after_external_commit_does_not_revert_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        repo.save_memory(&make_memory("doomed", "bye", 1_700_000_100))
            .await
            .expect("save");

        commit_out_of_band(&dir, "outsider.md", "not pali's file\n");

        repo.delete_memory("doomed", &Scope::Root)
            .await
            .expect("delete");

        assert!(
            in_head_tree(&dir, "outsider.md"),
            "#365: delete_memory reverted an out-of-band commit"
        );
        assert!(
            repo.read_memory("doomed", &Scope::Root).await.is_err(),
            "the delete itself must still take effect"
        );
    }

    #[tokio::test]
    async fn move_after_external_commit_does_not_revert_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        repo.save_memory(&make_memory("travelling", "here", 1_700_000_100))
            .await
            .expect("save");

        commit_out_of_band(&dir, "outsider.md", "not pali's file\n");

        let dest_name: MemoryName = "travelling".parse().expect("valid memory name");
        let dest_scope = Scope::Path(ScopePath::new("elsewhere").expect("valid scope path"));
        repo.move_memory("travelling", &Scope::Root, &dest_name, &dest_scope)
            .await
            .expect("move");

        assert!(
            in_head_tree(&dir, "outsider.md"),
            "#365: move_memory reverted an out-of-band commit"
        );
        repo.read_memory("travelling", &dest_scope)
            .await
            .expect("the move itself must still take effect: memory at destination");
        assert!(
            repo.read_memory("travelling", &Scope::Root).await.is_err(),
            "the move itself must still take effect: source removed"
        );
    }

    /// Path of `name` in the root scope, relative to the repository root.
    fn root_rel_path(repo: &MemoryRepo, name: &str) -> std::path::PathBuf {
        repo.memory_path(name, &Scope::Root)
            .strip_prefix(&repo.root)
            .expect("memory path is inside the repo")
            .to_path_buf()
    }

    /// The harm #365 actually reported: an outside commit that deletes one of
    /// pali's files and edits another must survive the next save, along with
    /// an outside add — no resurrected deletion, no reverted edit.
    #[tokio::test]
    async fn save_after_external_delete_and_edit_does_not_revert_them() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        repo.save_memory(&make_memory("keep", "one", 1_700_000_100))
            .await
            .expect("save keep");
        repo.save_memory(&make_memory("gone", "two", 1_700_000_200))
            .await
            .expect("save gone");

        let keep_rel = root_rel_path(&repo, "keep");
        let gone_rel = root_rel_path(&repo, "gone");
        let edited = b"edited outside pali\n";

        // One outside commit: edit `keep`, delete `gone`, add `outsider.md`.
        {
            let outside = git2::Repository::open(dir.path()).expect("open outside");
            let mut index = outside.index().expect("index");
            std::fs::write(dir.path().join(&keep_rel), edited).expect("edit keep");
            index.add_path(&keep_rel).expect("stage edit");
            std::fs::remove_file(dir.path().join(&gone_rel)).expect("delete gone");
            index.remove_path(&gone_rel).expect("stage delete");
            std::fs::write(dir.path().join("outsider.md"), "not pali's file\n").expect("add");
            index
                .add_path(std::path::Path::new("outsider.md"))
                .expect("stage add");
            index.write().expect("write index");
            let tree = outside
                .find_tree(index.write_tree().expect("write_tree"))
                .expect("find_tree");
            let sig = git2::Signature::now("outsider", "outsider@example.invalid").unwrap();
            let parent = outside.head().unwrap().peel_to_commit().unwrap();
            outside
                .commit(Some("HEAD"), &sig, &sig, "outside", &tree, &[&parent])
                .expect("outside commit");
        }

        repo.save_memory(&make_memory("third", "three", 1_700_000_300))
            .await
            .expect("save third");

        let git = git2::Repository::open(dir.path()).expect("open for checks");
        let tree = git.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(std::path::Path::new("outsider.md")).is_ok(),
            "#365: outside add reverted"
        );
        assert!(
            tree.get_path(&gone_rel).is_err(),
            "#365: outside delete reverted (file resurrected in HEAD)"
        );
        let keep_blob = git
            .find_blob(tree.get_path(&keep_rel).expect("keep in HEAD").id())
            .unwrap();
        assert_eq!(
            keep_blob.content(),
            edited,
            "#365: outside edit reverted in HEAD"
        );
        assert!(
            tree.get_path(&root_rel_path(&repo, "third")).is_ok(),
            "the save itself must still take effect"
        );
        assert_eq!(
            git.diff_tree_to_index(Some(&tree), None, None)
                .unwrap()
                .deltas()
                .len(),
            0,
            "on-disk index differs from HEAD after the save"
        );
        assert_eq!(
            git.diff_index_to_workdir(None, None)
                .unwrap()
                .deltas()
                .len(),
            0,
            "working tree differs from the index after the save"
        );
    }

    /// A branch ref removed from outside leaves HEAD unborn. The next save
    /// must not commit a root that holds only its own file: the memories
    /// already tracked have to be in that commit too.
    #[tokio::test]
    async fn save_after_branch_ref_removed_keeps_tracked_memories() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        repo.save_memory(&make_memory("first", "one", 1_700_000_100))
            .await
            .expect("first save");

        {
            let outside = git2::Repository::open(dir.path()).expect("open outside");
            let mut branch = outside.head().expect("head").resolve().expect("resolve");
            branch.delete().expect("delete branch ref");
            assert!(outside.head().is_err(), "precondition: HEAD is unborn");
        }

        repo.save_memory(&make_memory("second", "two", 1_700_000_200))
            .await
            .expect("second save");

        let git = git2::Repository::open(dir.path()).expect("open for checks");
        let tree = git.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(&root_rel_path(&repo, "second")).is_ok(),
            "the save itself must take effect"
        );
        assert!(
            tree.get_path(&root_rel_path(&repo, "first")).is_ok(),
            "an unborn HEAD cleared the index: the new root commit dropped \
             every memory already tracked"
        );
    }

    // -- redact_url tests --------------------------------------------------

    #[test]
    fn redact_url_strips_userinfo() {
        assert_eq!(
            redact_url("https://user:ghp_token123@github.com/org/repo.git"),
            "https://[REDACTED]@github.com/org/repo.git"
        );
    }

    #[test]
    fn redact_url_no_at_passthrough() {
        let url = "https://github.com/org/repo.git";
        assert_eq!(redact_url(url), url);
    }

    #[test]
    fn redact_url_file_protocol_passthrough() {
        let url = "file:///tmp/bare.git";
        assert_eq!(redact_url(url), url);
    }

    // -- assert_within_root tests ------------------------------------------

    #[test]
    fn assert_within_root_accepts_valid_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::init_or_open(dir.path(), None).unwrap();
        let valid = dir.path().join("global").join("my-memory.md");
        // Create the parent so canonicalization works.
        std::fs::create_dir_all(valid.parent().unwrap()).unwrap();
        assert!(repo.assert_within_root(&valid).is_ok());
    }

    #[test]
    fn assert_within_root_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::init_or_open(dir.path(), None).unwrap();
        // Build a path that escapes the repo root. We need enough ".." to go
        // above the tmpdir, then descend into /tmp/evil.
        let _evil = dir
            .path()
            .join("..")
            .join("..")
            .join("..")
            .join("tmp")
            .join("evil.md");
        // Only assert if the path actually resolves outside root.
        // (If the temp dir is at root level, this might not escape — use an
        // explicit absolute path instead.)
        let outside = std::path::PathBuf::from("/tmp/definitely-outside");
        assert!(repo.assert_within_root(&outside).is_err());
    }

    // -- local-only mode tests (no origin) ---------------------------------

    #[tokio::test]
    async fn push_local_only_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);
        let auth = test_auth();
        // No origin configured — push should silently succeed.
        let result = repo.push(&auth, "main").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pull_local_only_returns_no_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);
        let auth = test_auth();
        let result = repo.pull(&auth, "main").await.unwrap();
        assert!(matches!(result, PullResult::NoRemote));
    }

    // -- push/pull with local bare remote ----------------------------------

    #[tokio::test]
    async fn push_to_bare_remote() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let local_dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&local_dir, Some(&remote_url));
        let auth = test_auth();

        // Save a memory so there's something to push.
        let mem = make_memory("test-push", "push content", 1_700_000_000);
        repo.save_memory(&mem).await.unwrap();

        // Push should succeed.
        repo.push(&auth, "main").await.unwrap();

        // Verify the bare repo received the commit.
        let bare = git2::Repository::open_bare(_remote_dir.path()).unwrap();
        let head = bare.find_reference("refs/heads/main").unwrap();
        let commit = head.peel_to_commit().unwrap();
        assert!(commit.message().unwrap().contains("test-push"));
    }

    #[tokio::test]
    async fn pull_from_empty_bare_remote_returns_up_to_date() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let local_dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&local_dir, Some(&remote_url));
        let auth = test_auth();

        // First save something locally so we have an initial commit (HEAD exists).
        let mem = make_memory("seed", "seed content", 1_700_000_000);
        repo.save_memory(&mem).await.unwrap();

        // Pull from empty remote — should be up-to-date (not an error).
        let result = repo.pull(&auth, "main").await.unwrap();
        assert!(matches!(result, PullResult::UpToDate));
    }

    #[tokio::test]
    async fn pull_fast_forward() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let auth = test_auth();

        // Repo A: save and push
        let dir_a = tempfile::tempdir().unwrap();
        let repo_a = open_repo(&dir_a, Some(&remote_url));
        let mem = make_memory("from-a", "content from A", 1_700_000_000);
        repo_a.save_memory(&mem).await.unwrap();
        repo_a.push(&auth, "main").await.unwrap();

        // Repo B: init with same remote, then pull
        let dir_b = tempfile::tempdir().unwrap();
        let repo_b = open_repo(&dir_b, Some(&remote_url));
        // Repo B needs an initial commit for HEAD to exist.
        let seed = make_memory("seed-b", "seed", 1_700_000_000);
        repo_b.save_memory(&seed).await.unwrap();

        let result = repo_b.pull(&auth, "main").await.unwrap();
        assert!(
            matches!(
                result,
                PullResult::FastForward { .. } | PullResult::Merged { .. }
            ),
            "expected fast-forward or merge, got {:?}",
            result
        );

        // Verify the memory file from A exists in B's working directory.
        let file = dir_b.path().join("global").join("from-a.md");
        assert!(file.exists(), "from-a.md should exist in repo B after pull");
    }

    #[tokio::test]
    async fn pull_up_to_date_after_push() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let local_dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&local_dir, Some(&remote_url));
        let auth = test_auth();

        let mem = make_memory("synced", "synced content", 1_700_000_000);
        repo.save_memory(&mem).await.unwrap();
        repo.push(&auth, "main").await.unwrap();

        // Pull immediately after push — should be up to date.
        let result = repo.pull(&auth, "main").await.unwrap();
        assert!(matches!(result, PullResult::UpToDate));
    }

    // -- conflict resolution tests -----------------------------------------

    #[tokio::test]
    async fn pull_merge_conflict_theirs_newer_wins() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let auth = test_auth();

        // Repo A: save "shared" with T1, push
        let dir_a = tempfile::tempdir().unwrap();
        let repo_a = open_repo(&dir_a, Some(&remote_url));
        let mem_a1 = make_memory("shared", "version from A initial", 1_700_000_100);
        repo_a.save_memory(&mem_a1).await.unwrap();
        repo_a.push(&auth, "main").await.unwrap();

        // Repo B: pull to get A's commit, then modify "shared" with T3 (newer), push
        let dir_b = tempfile::tempdir().unwrap();
        let repo_b = open_repo(&dir_b, Some(&remote_url));
        let seed = make_memory("seed-b", "seed", 1_700_000_000);
        repo_b.save_memory(&seed).await.unwrap();
        repo_b.pull(&auth, "main").await.unwrap();

        let mem_b = make_memory("shared", "version from B (newer)", 1_700_000_300);
        repo_b.save_memory(&mem_b).await.unwrap();
        repo_b.push(&auth, "main").await.unwrap();

        // Repo A: modify "shared" with T2 (older than T3), then pull — conflict
        let mem_a2 = make_memory("shared", "version from A (older)", 1_700_000_200);
        repo_a.save_memory(&mem_a2).await.unwrap();
        let result = repo_a.pull(&auth, "main").await.unwrap();

        assert!(
            matches!(result, PullResult::Merged { conflicts_resolved, .. } if conflicts_resolved >= 1),
            "expected merge with conflicts resolved, got {:?}",
            result
        );

        // Verify theirs (B's version, T3=300) won.
        let file = dir_a.path().join("global").join("shared.md");
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("version from B (newer)"),
            "expected B's version to win (newer timestamp), got: {}",
            content
        );
    }

    #[tokio::test]
    async fn pull_merge_conflict_ours_newer_wins() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let auth = test_auth();

        // Repo A: save "shared" with T1, push
        let dir_a = tempfile::tempdir().unwrap();
        let repo_a = open_repo(&dir_a, Some(&remote_url));
        let mem_a1 = make_memory("shared", "version from A initial", 1_700_000_100);
        repo_a.save_memory(&mem_a1).await.unwrap();
        repo_a.push(&auth, "main").await.unwrap();

        // Repo B: pull, modify with T2 (older), push
        let dir_b = tempfile::tempdir().unwrap();
        let repo_b = open_repo(&dir_b, Some(&remote_url));
        let seed = make_memory("seed-b", "seed", 1_700_000_000);
        repo_b.save_memory(&seed).await.unwrap();
        repo_b.pull(&auth, "main").await.unwrap();

        let mem_b = make_memory("shared", "version from B (older)", 1_700_000_200);
        repo_b.save_memory(&mem_b).await.unwrap();
        repo_b.push(&auth, "main").await.unwrap();

        // Repo A: modify with T3 (newer), pull — conflict
        let mem_a2 = make_memory("shared", "version from A (newer)", 1_700_000_300);
        repo_a.save_memory(&mem_a2).await.unwrap();
        let result = repo_a.pull(&auth, "main").await.unwrap();

        assert!(
            matches!(result, PullResult::Merged { conflicts_resolved, .. } if conflicts_resolved >= 1),
            "expected merge with conflicts resolved, got {:?}",
            result
        );

        // Verify ours (A's version, T3=300) won.
        let file = dir_a.path().join("global").join("shared.md");
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("version from A (newer)"),
            "expected A's version to win (newer timestamp), got: {}",
            content
        );
    }

    #[tokio::test]
    async fn pull_merge_no_conflict_different_files() {
        let (_remote_dir, remote_url) = setup_bare_remote();
        let auth = test_auth();

        // Repo A: save "mem-a", push
        let dir_a = tempfile::tempdir().unwrap();
        let repo_a = open_repo(&dir_a, Some(&remote_url));
        let mem_a = make_memory("mem-a", "from A", 1_700_000_100);
        repo_a.save_memory(&mem_a).await.unwrap();
        repo_a.push(&auth, "main").await.unwrap();

        // Repo B: pull, save "mem-b", push
        let dir_b = tempfile::tempdir().unwrap();
        let repo_b = open_repo(&dir_b, Some(&remote_url));
        let seed = make_memory("seed-b", "seed", 1_700_000_000);
        repo_b.save_memory(&seed).await.unwrap();
        repo_b.pull(&auth, "main").await.unwrap();
        let mem_b = make_memory("mem-b", "from B", 1_700_000_200);
        repo_b.save_memory(&mem_b).await.unwrap();
        repo_b.push(&auth, "main").await.unwrap();

        // Repo A: save "mem-a2" (different file), pull — should merge cleanly
        let mem_a2 = make_memory("mem-a2", "also from A", 1_700_000_300);
        repo_a.save_memory(&mem_a2).await.unwrap();
        let result = repo_a.pull(&auth, "main").await.unwrap();

        assert!(
            matches!(
                result,
                PullResult::Merged {
                    conflicts_resolved: 0,
                    ..
                }
            ),
            "expected clean merge, got {:?}",
            result
        );

        // Both repos should have all files.
        assert!(dir_a.path().join("global").join("mem-b.md").exists());
    }

    // -- diff_changed_memories tests ----------------------------------------

    /// Helper: valid on-disk memory markdown (frontmatter + body) for diff
    /// tests — changed files are resolved from their frontmatter.
    fn memory_markdown(name: &str, scope: Scope, content: &str) -> String {
        Memory::new(name, content, MemoryMetadata::new(scope, vec![], None))
            .unwrap()
            .to_markdown()
            .unwrap()
    }

    /// Helper: the expected resolved reference for a memory.
    fn mref(scope: Scope, name: &str) -> crate::types::MemoryRef {
        crate::types::MemoryRef::new(scope, MemoryName::new(name).unwrap())
    }

    /// Helper: commit a file with given content and return the new HEAD OID bytes.
    fn commit_file(repo: &Arc<MemoryRepo>, rel_path: &str, content: &str) -> [u8; 20] {
        let inner = repo.inner.lock().expect("lock poisoned");
        let full_path = repo.root.join(rel_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();

        let mut index = inner.index().unwrap();
        index.add_path(std::path::Path::new(rel_path)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = inner.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();

        let oid = match inner.head() {
            Ok(head) => {
                let parent = head.peel_to_commit().unwrap();
                inner
                    .commit(Some("HEAD"), &sig, &sig, "test commit", &tree, &[&parent])
                    .unwrap()
            }
            Err(_) => inner
                .commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
                .unwrap(),
        };

        let mut buf = [0u8; 20];
        buf.copy_from_slice(oid.as_bytes());
        buf
    }

    /// Helper: commit raw bytes (which may be non-UTF-8) at `rel_path` and
    /// return the new HEAD OID bytes. Used to exercise the published
    /// `diff_changed_memories` path contract against blobs that cannot be
    /// resolved to a memory.
    fn commit_bytes(repo: &Arc<MemoryRepo>, rel_path: &str, content: &[u8]) -> [u8; 20] {
        let inner = repo.inner.lock().expect("lock poisoned");
        let full_path = repo.root.join(rel_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();

        let mut index = inner.index().unwrap();
        index.add_path(std::path::Path::new(rel_path)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = inner.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();

        let oid = match inner.head() {
            Ok(head) => {
                let parent = head.peel_to_commit().unwrap();
                inner
                    .commit(Some("HEAD"), &sig, &sig, "test commit", &tree, &[&parent])
                    .unwrap()
            }
            Err(_) => inner
                .commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
                .unwrap(),
        };

        let mut buf = [0u8; 20];
        buf.copy_from_slice(oid.as_bytes());
        buf
    }

    /// Helper: replace `rel_path` with a symlink pointing at `target` and
    /// commit it. Records a git symlink entry (mode `120000`) so a preceding
    /// regular-file commit at the same path yields a `Delta::Typechange`.
    fn commit_symlink(repo: &Arc<MemoryRepo>, rel_path: &str, target: &str) -> [u8; 20] {
        let inner = repo.inner.lock().expect("lock poisoned");
        let full_path = repo.root.join(rel_path);
        // Remove any existing regular file, then create the symlink on disk.
        let _ = std::fs::remove_file(&full_path);
        std::os::unix::fs::symlink(target, &full_path).unwrap();

        let mut index = inner.index().unwrap();
        index.add_path(std::path::Path::new(rel_path)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = inner.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let parent = inner.head().unwrap().peel_to_commit().unwrap();
        let oid = inner
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "typechange to symlink",
                &tree,
                &[&parent],
            )
            .unwrap();

        let mut buf = [0u8; 20];
        buf.copy_from_slice(oid.as_bytes());
        buf
    }

    #[test]
    fn diff_changed_memories_detects_added_global() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        // Capture the initial HEAD (init commit).
        let old_oid = {
            let inner = repo.inner.lock().unwrap();
            let head = inner.head().unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(head.peel_to_commit().unwrap().id().as_bytes());
            buf
        };

        let new_oid = commit_file(
            &repo,
            "global/my-note.md",
            &memory_markdown("my-note", Scope::Root, "# content"),
        );

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert_eq!(changes.upserted, vec![mref(Scope::Root, "my-note")]);
        assert!(changes.removed.is_empty());
        assert_eq!(changes.unresolved, 0);
    }

    #[test]
    fn diff_changed_memories_detects_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let first_oid = commit_file(
            &repo,
            "global/to-delete.md",
            &memory_markdown("to-delete", Scope::Root, "hello"),
        );
        let second_oid = {
            let inner = repo.inner.lock().unwrap();
            let full_path = dir.path().join("global/to-delete.md");
            std::fs::remove_file(&full_path).unwrap();
            let mut index = inner.index().unwrap();
            index
                .remove_path(std::path::Path::new("global/to-delete.md"))
                .unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = inner.find_tree(tree_oid).unwrap();
            let sig = git2::Signature::now("test", "test@test.com").unwrap();
            let parent = inner.head().unwrap().peel_to_commit().unwrap();
            let oid = inner
                .commit(Some("HEAD"), &sig, &sig, "delete file", &tree, &[&parent])
                .unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(oid.as_bytes());
            buf
        };

        let changes = repo.diff_changed_refs(first_oid, second_oid).unwrap();
        assert!(changes.upserted.is_empty());
        // The removal is resolved from the *old* tree's frontmatter — the
        // file no longer exists in the new tree or the working directory.
        assert_eq!(changes.removed, vec![mref(Scope::Root, "to-delete")]);
        assert_eq!(changes.unresolved, 0);
    }

    #[test]
    fn diff_changed_memories_ignores_non_md_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let old_oid = {
            let inner = repo.inner.lock().unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(
                inner
                    .head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .id()
                    .as_bytes(),
            );
            buf
        };

        // Add a non-.md file under global/ and a .md file outside tracked dirs.
        let _ = commit_file(&repo, "global/config.json", "{}");
        let new_oid = commit_file(&repo, "other/note.md", "# ignored");

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert!(
            changes.upserted.is_empty(),
            "should ignore non-.md and out-of-scope files"
        );
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn diff_changed_memories_detects_modified() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let scope: Scope = "myproject".parse().unwrap();
        let first_oid = commit_file(
            &repo,
            "projects/myproject/note.md",
            &memory_markdown("note", scope.clone(), "version 1"),
        );
        let second_oid = commit_file(
            &repo,
            "projects/myproject/note.md",
            &memory_markdown("note", scope.clone(), "version 2"),
        );

        let changes = repo.diff_changed_refs(first_oid, second_oid).unwrap();
        assert_eq!(changes.upserted, vec![mref(scope, "note")]);
        assert!(changes.removed.is_empty());
        assert_eq!(changes.unresolved, 0);
    }

    /// Hierarchical scopes make on-disk paths ambiguous
    /// (`projects/a/b/mem.md` could be scope `a/b`, name `mem` or scope
    /// `a`, name `b/mem`); resolution must come from the frontmatter, never
    /// from splitting the path.
    #[test]
    fn diff_changed_memories_resolves_hierarchical_scope_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let old_oid = {
            let inner = repo.inner.lock().unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(
                inner
                    .head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .id()
                    .as_bytes(),
            );
            buf
        };

        let scope: Scope = "a/b".parse().unwrap();
        let new_oid = commit_file(
            &repo,
            "projects/a/b/mem.md",
            &memory_markdown("mem", scope.clone(), "hierarchical content"),
        );

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert_eq!(
            changes.upserted,
            vec![mref(scope, "mem")],
            "must resolve scope 'a/b' + name 'mem' from frontmatter, not \
             scope 'a' + name 'b/mem' from the path"
        );
        assert_eq!(changes.unresolved, 0);
    }

    /// A changed `.md` file that is not a parseable memory must be counted
    /// as unresolved — never silently dropped from the change set.
    #[test]
    fn diff_changed_memories_counts_unparseable_files_as_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let old_oid = {
            let inner = repo.inner.lock().unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(
                inner
                    .head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .id()
                    .as_bytes(),
            );
            buf
        };

        let _ = commit_file(&repo, "global/broken.md", "no frontmatter at all");
        let new_oid = commit_file(
            &repo,
            "global/fine.md",
            &memory_markdown("fine", Scope::Root, "resolvable"),
        );

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert_eq!(changes.upserted, vec![mref(Scope::Root, "fine")]);
        assert!(changes.removed.is_empty());
        assert_eq!(changes.unresolved, 1);
    }

    /// A zero OID (unborn branch sentinel) must not crash; all files in the
    /// new commit should appear as additions.
    #[test]
    fn diff_changed_memories_zero_oid_treats_all_as_added() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        // Commit a global memory file — this is the "new" state.
        let new_oid = commit_file(
            &repo,
            "global/first-memory.md",
            &memory_markdown("first-memory", Scope::Root, "# Hello"),
        );

        // old_oid = [0u8; 20] simulates an unborn branch (no prior commit).
        let old_oid = [0u8; 20];

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert_eq!(
            changes.upserted,
            vec![mref(Scope::Root, "first-memory")],
            "zero OID: all new-tree files should be additions"
        );
        assert!(changes.removed.is_empty(), "zero OID: no removals expected");
    }

    /// Replace a tracked regular memory file with a symlink at the same path
    /// (git raw status `T`). `list_memories` skips symlinks, so the memory
    /// leaves authoritative repository truth — the diff must therefore emit a
    /// removal of the old memory (so its lexical/vector entry disappears) and
    /// count the unresolvable symlink new-side, forcing degrade + repair.
    /// Without `Delta::Typechange` handling the change is dropped entirely and
    /// the old entry stays stale-Available.
    #[test]
    fn diff_changed_refs_regular_to_symlink_removes_old_and_flags_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        // Commit the memory as a normal file.
        let old_oid = commit_file(
            &repo,
            "global/note.md",
            &memory_markdown("note", Scope::Root, "noteword content"),
        );

        // Replace it with a symlink at the same path and commit the typechange.
        let new_oid = commit_symlink(&repo, "global/note.md", "elsewhere");

        let changes = repo.diff_changed_refs(old_oid, new_oid).unwrap();
        assert_eq!(
            changes.removed,
            vec![mref(Scope::Root, "note")],
            "regular->symlink must remove the old memory's canonical key"
        );
        assert!(
            changes.upserted.is_empty(),
            "the symlink new-side is not a resolvable memory, so nothing is upserted"
        );
        assert_eq!(
            changes.unresolved, 1,
            "the symlink new-side must be counted unresolved so the index degrades"
        );
    }

    /// Published-contract regression guard for `diff_changed_memories`.
    ///
    /// This is the stable 0.16.0 public surface, distinct from the internal
    /// `diff_changed_refs` mirror path. It must return **repository-path**
    /// strings without the `.md` suffix (e.g. `projects/a/b/mem`,
    /// `global/broken`), derived straight from git deltas — NOT canonical
    /// resolved keys like `v1:scope=a/b;name=mem`. And because a path is
    /// available even when the blob is not a parseable memory, it must report
    /// **every** changed `.md` file, including unparseable and non-UTF-8 ones,
    /// rather than dropping them the way the resolving `diff_changed_refs`
    /// does. `cargo-semver-checks` cannot see returned-value semantics, so this
    /// test is the behavior-compat guard: it fails if the public method ever
    /// again projects canonical keys or drops unresolvable files.
    #[test]
    fn diff_changed_memories_returns_repo_paths_including_unresolvable_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir, None);

        let old_oid = {
            let inner = repo.inner.lock().unwrap();
            let mut buf = [0u8; 20];
            buf.copy_from_slice(
                inner
                    .head()
                    .unwrap()
                    .peel_to_commit()
                    .unwrap()
                    .id()
                    .as_bytes(),
            );
            buf
        };

        // A resolvable memory in a hierarchical scope: exercises that the
        // public method returns the on-disk path, not the canonical key.
        let scope: Scope = "a/b".parse().unwrap();
        let _ = commit_file(
            &repo,
            "projects/a/b/mem.md",
            &memory_markdown("mem", scope.clone(), "hierarchical content"),
        );

        // A `.md` file with no frontmatter — cannot resolve to a memory.
        let _ = commit_file(&repo, "global/broken.md", "no frontmatter at all");

        // A non-UTF-8 `.md` blob — cannot even be read as a string.
        let new_oid = commit_bytes(&repo, "global/binary.md", &[0xff, 0xfe, 0x00, 0x9f]);

        let changes = repo.diff_changed_memories(old_oid, new_oid).unwrap();

        let mut upserted = changes.upserted.clone();
        upserted.sort();
        assert_eq!(
            upserted,
            vec![
                "global/binary".to_string(),
                "global/broken".to_string(),
                "projects/a/b/mem".to_string(),
            ],
            "public method must return repo-path strings (not canonical keys) \
             and must include unparseable + non-UTF-8 files, not drop them"
        );

        // Guard the exact contrast the round-five finding is about: the
        // hierarchical file must NOT appear as a canonical resolved key.
        assert!(
            !changes
                .upserted
                .contains(&mref(scope, "mem").qualified_path()),
            "public method must not project canonical resolved keys"
        );

        assert!(changes.removed.is_empty());
    }
}
