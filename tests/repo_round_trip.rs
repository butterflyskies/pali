//! Integration tests for the memory repository — save, read, list, pull, delete, move.
//!
//! Uses `AuthProvider::with_token` to inject a known token without needing
//! real credentials. The `push_pull_with_bare_remote` test exercises the full
//! credential callback path through a `file://` bare remote.

use std::sync::Arc;

use pali::auth::AuthProvider;
#[cfg(unix)]
use pali::error::MemoryError;
use pali::repo::MemoryRepo;
use pali::types::{Memory, MemoryMetadata, MemoryName, PullResult, Scope};

/// Full round-trip: init repo → save memory → read it back → list → delete → pull.
///
/// `pull` exercises the auth path: `AuthProvider::with_token` provides a
/// pre-seeded token, but because there's no remote configured the pull
/// returns `PullResult::NoRemote` without ever hitting the network.
/// This proves the `with_token` constructor works end-to-end through
/// the repo layer.
#[tokio::test]
async fn repo_round_trip_with_test_auth_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let repo =
        Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("should init fresh repo"));
    let auth = AuthProvider::with_token("ghp_fake_test_token");

    // Save a memory.
    let metadata = MemoryMetadata::new(Scope::Root, vec!["test".into()], None);
    let memory = Memory::new("test-memory", "Hello from integration test.", metadata).unwrap();
    repo.save_memory(&memory)
        .await
        .expect("save should succeed");

    // Read it back.
    let loaded = repo
        .read_memory("test-memory", &Scope::Root)
        .await
        .expect("read should find the memory");
    assert_eq!(loaded.name.as_str(), "test-memory");
    assert_eq!(loaded.content, "Hello from integration test.");

    // List all memories.
    let list = repo.list_memories(None).await.expect("list should succeed");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.as_str(), "test-memory");

    // Pull with no remote — exercises auth resolution path via with_token.
    let pull_result = repo.pull(&auth, "main").await.expect("pull should succeed");
    assert!(
        matches!(pull_result, PullResult::NoRemote),
        "expected NoRemote since we didn't configure origin, got: {pull_result:?}",
    );

    // Delete the memory.
    repo.delete_memory("test-memory", &Scope::Root)
        .await
        .expect("delete should succeed");

    // Verify it's gone.
    let list_after = repo.list_memories(None).await.expect("list should succeed");
    assert!(list_after.is_empty(), "memory should be deleted");
}

/// Push and pull against a `file://` bare remote, exercising the full
/// credential callback path through `AuthProvider::with_token`.
///
/// `file://` remotes don't actually check credentials, but the git2
/// `RemoteCallbacks::credentials` closure is still invoked — this proves
/// the token flows from `AuthProvider` through `build_auth_callbacks` into
/// the git transport layer without panicking or type errors.
#[tokio::test]
async fn push_pull_with_bare_remote() {
    // Set up a bare remote.
    let remote_dir = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(remote_dir.path()).expect("failed to init bare repo");
    let remote_url = format!("file://{}", remote_dir.path().display());

    // Init a local repo with origin pointing at the bare remote.
    let local_dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        MemoryRepo::init_or_open(local_dir.path(), Some(&remote_url))
            .expect("should init local repo with remote"),
    );
    let auth = AuthProvider::with_token("ghp_fake_push_token");

    // Save a memory so there's something to push.
    let metadata = MemoryMetadata::new(Scope::Root, vec!["push-test".into()], None);
    let memory = Memory::new("push-memory", "Content for push test.", metadata).unwrap();
    repo.save_memory(&memory)
        .await
        .expect("save should succeed");

    // Push to the bare remote.
    repo.push(&auth, "main").await.expect("push should succeed");

    // Verify the bare remote received the commit.
    let bare = git2::Repository::open_bare(remote_dir.path()).unwrap();
    let head = bare.find_reference("refs/heads/main").unwrap();
    let commit = head.peel_to_commit().unwrap();
    assert!(
        commit.message().unwrap().contains("push-memory"),
        "bare remote should contain a commit mentioning the memory name",
    );

    // Pull into a fresh clone to verify the round-trip.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_repo = Arc::new(
        MemoryRepo::init_or_open(clone_dir.path(), Some(&remote_url))
            .expect("should init clone repo"),
    );
    let clone_auth = AuthProvider::with_token("ghp_fake_pull_token");
    let pull_result = clone_repo
        .pull(&clone_auth, "main")
        .await
        .expect("pull should succeed");
    assert!(
        matches!(
            pull_result,
            PullResult::FastForward { .. } | PullResult::Merged { .. }
        ),
        "expected FastForward or Merged pull, got: {pull_result:?}",
    );

    // The cloned repo should have the memory.
    let memories = clone_repo
        .list_memories(None)
        .await
        .expect("list should succeed");
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].name.as_str(), "push-memory");
    assert_eq!(memories[0].content, "Content for push test.");
}

/// Push via `git://` to a daemon whose bare remote has an `update` hook
/// that rejects all ref updates.
///
/// This exercises the `push_update_reference` callback path — the fix for
/// issue #81. Unlike `file://` transport, the git smart protocol runs
/// server-side hooks and reports per-ref rejection status through the
/// callback. Our code collects those rejections and surfaces them as
/// `MemoryError::PushRejected`.
#[cfg(unix)]
#[tokio::test]
async fn push_rejected_by_server_hook() {
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Command, Stdio};

    /// RAII guard that kills `git daemon` on drop.
    struct GitDaemon(Child);
    impl Drop for GitDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Grab a free port, then release it immediately. The TOCTOU window
    // before the daemon binds is negligible in test environments.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // Set up a bare remote with an `update` hook that rejects all pushes.
    let remote_dir = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(remote_dir.path()).expect("failed to init bare repo");

    let hooks_dir = remote_dir.path().join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("update");
    fs::write(
        &hook_path,
        "#!/bin/sh\necho \"branch protection: PRs required\" >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Start git daemon with --verbose so it prints "Ready to rumble" to
    // stderr once it has bound the port and is accepting connections.
    let base_path = remote_dir.path().parent().unwrap();
    let mut child = Command::new("git")
        .args([
            "daemon",
            "--verbose",
            "--reuseaddr",
            "--listen=127.0.0.1",
            &format!("--port={port}"),
            &format!("--base-path={}", base_path.display()),
            "--enable=receive-pack",
            "--export-all",
            &base_path.to_string_lossy(),
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("failed to start git daemon — is git installed?");

    // Block until the daemon confirms it's ready, then keep draining stderr
    // in a background thread so verbose logging doesn't fill the pipe buffer
    // and block the daemon mid-connection.
    let stderr = child.stderr.take().unwrap();
    let _daemon = GitDaemon(child);

    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();
    let mut ready = false;
    for line in &mut lines {
        if line.unwrap_or_default().contains("Ready to rumble") {
            ready = true;
            break;
        }
    }
    assert!(ready, "git daemon exited without becoming ready");

    // Drain remaining stderr in the background so the daemon doesn't stall.
    std::thread::spawn(move || for _ in lines {});

    // Build the git:// URL pointing at the bare repo.
    let repo_name = remote_dir.path().file_name().unwrap().to_string_lossy();
    let remote_url = format!("git://127.0.0.1:{port}/{repo_name}");

    // Init a local repo, save a memory, and push.
    let local_dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        MemoryRepo::init_or_open(local_dir.path(), Some(&remote_url))
            .expect("should init local repo"),
    );
    let auth = AuthProvider::with_token("ghp_fake_token");

    let metadata = MemoryMetadata::new(Scope::Root, vec!["rejected".into()], None);
    let memory = Memory::new("rejected-memory", "This push should be rejected.", metadata).unwrap();
    repo.save_memory(&memory)
        .await
        .expect("save should succeed");

    let result = repo.push(&auth, "main").await;
    assert!(result.is_err(), "push to a rejecting remote should fail");
    let err = result.unwrap_err();
    assert!(
        matches!(err, MemoryError::PushRejected(_)),
        "expected PushRejected, got: {err:?}",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("refs/heads/main"),
        "rejection should name the rejected ref, got: {msg}",
    );
}

/// Move a memory to a different scope with a new name via `repo.move_memory`.
#[tokio::test]
async fn move_memory_with_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let repo =
        Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("should init fresh repo"));

    let from_scope: Scope = "project-a".parse().unwrap();
    let to_scope: Scope = "project-b".parse().unwrap();

    // Save a memory.
    let metadata = MemoryMetadata::new(from_scope.clone(), vec!["rename-test".into()], None);
    let memory = Memory::new("old-name", "Content for rename test.", metadata).unwrap();
    repo.save_memory(&memory)
        .await
        .expect("save should succeed");

    let new_name = MemoryName::new("new-name").unwrap();
    repo.move_memory("old-name", &from_scope, &new_name, &to_scope)
        .await
        .expect("move should succeed");

    // Old name in old scope is gone.
    assert!(repo.read_memory("old-name", &from_scope).await.is_err());

    // New name in new scope exists with correct content.
    let loaded = repo
        .read_memory("new-name", &to_scope)
        .await
        .expect("renamed memory should exist");
    assert_eq!(loaded.content, "Content for rename test.");
    assert_eq!(loaded.metadata.tags, vec!["rename-test".to_string()]);
}

/// Exercise `repo.move_memory` directly — single-commit atomicity.
///
/// Verifies that after a successful move the source is gone, the
/// destination exists with correct content, and the git log shows
/// exactly one new commit (not two).
#[tokio::test]
async fn repo_move_memory_atomic_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let repo =
        Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("should init fresh repo"));

    let from_scope = Scope::Root;
    let to_scope: Scope = "target-project".parse().unwrap();

    // Save source memory (creates first commit after init).
    let metadata = MemoryMetadata::new(
        from_scope.clone(),
        vec!["atomic".into()],
        Some("test".into()),
    );
    let source = Memory::new("atomic-mem", "Atomic move content.", metadata).unwrap();
    repo.save_memory(&source)
        .await
        .expect("save should succeed");

    // Count commits before move.
    let pre_move_commits = {
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut revwalk = git.revwalk().unwrap();
        revwalk.push_head().unwrap();
        revwalk.count()
    };

    // Move via the repo method.
    let dest_name = MemoryName::new("atomic-mem").unwrap();
    let dest = repo
        .move_memory("atomic-mem", &from_scope, &dest_name, &to_scope)
        .await
        .expect("move should succeed");

    // Returned memory has correct content and metadata.
    assert_eq!(dest.content, "Atomic move content.");
    assert_eq!(dest.metadata.tags, vec!["atomic".to_string()]);
    assert_eq!(dest.metadata.source.as_deref(), Some("test"));

    // Source should be gone.
    assert!(
        repo.read_memory("atomic-mem", &from_scope).await.is_err(),
        "source should be deleted"
    );

    // Destination should be readable from repo.
    let loaded = repo
        .read_memory("atomic-mem", &to_scope)
        .await
        .expect("dest should exist");
    assert_eq!(loaded.content, "Atomic move content.");

    // Exactly one new commit should have been created (the move).
    let post_move_commits = {
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut revwalk = git.revwalk().unwrap();
        revwalk.push_head().unwrap();
        revwalk.count()
    };
    assert_eq!(
        post_move_commits,
        pre_move_commits + 1,
        "move should produce exactly one git commit"
    );
}

/// ADR-0041 compatibility (#293 review, round 6): a repo path spelled with
/// `..` over a not-yet-existing component — `existing/missing/../repo` —
/// worked before startup canonicalization was introduced, so the startup
/// pipeline (canonicalize, then init at the resolved path) must still accept
/// it and land the repo at `existing/repo`.
#[tokio::test]
async fn init_accepts_dot_dot_over_missing_component_in_repo_path() {
    let tmp = tempfile::tempdir().unwrap();
    let existing = tmp.path().canonicalize().unwrap();
    let spelled = tmp.path().join("missing/../repo");

    // Mirrors `run_serve`: canonicalize before opening.
    let resolved = pali::fs_util::canonicalize_allow_missing(&spelled)
        .expect("previously valid spelling must canonicalize");
    assert_eq!(resolved, existing.join("repo"));

    // Repo init at the resolved path succeeds and creates the directory.
    let repo = MemoryRepo::init_or_open(&resolved, None).expect("repo init should succeed");
    assert!(
        existing.join("repo/.git").exists(),
        "repo should be created"
    );
    drop(repo);
}
