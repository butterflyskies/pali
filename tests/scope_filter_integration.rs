//! Integration tests for scope-affinity filtering in list and recall.
//!
//! These tests exercise the repo layer directly with `list_memories` and
//! verify that the server-layer filtering pattern (match on ScopeFilter) works
//! as expected when applied to the full memory list.

use std::sync::Arc;

use pali::repo::MemoryRepo;
use pali::types::{Memory, MemoryMetadata, Scope, ScopePath};

fn path(s: &str) -> Scope {
    Scope::Path(ScopePath::new(s).unwrap())
}

/// Helper: initialise a fresh in-memory repo in a temp directory.
async fn make_repo() -> (Arc<MemoryRepo>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let repo =
        Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("should init fresh repo"));
    (repo, tmp)
}

/// Helper: save a memory with the given scope and name.
async fn save(repo: &Arc<MemoryRepo>, name: &str, scope: Scope) {
    let metadata = MemoryMetadata::new(scope, vec![], None);
    let memory = Memory::new(name, format!("Content for {}", name), metadata).unwrap();
    repo.save_memory(&memory)
        .await
        .expect("save should succeed");
}

// ---------------------------------------------------------------------------
// list_memories(Some(&Scope::Global)) — global-only
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_scope_filter_global_only() {
    let (repo, _tmp) = make_repo().await;

    save(&repo, "global-mem", Scope::Root).await;
    save(&repo, "proj-mem", path("test-proj")).await;

    let memories = repo
        .list_memories(Some(&Scope::Root))
        .await
        .expect("list should succeed");

    assert_eq!(memories.len(), 1, "expected only the global memory");
    assert_eq!(memories[0].name.as_str(), "global-mem");
    assert_eq!(memories[0].metadata.scope, Scope::Root);
}

// ---------------------------------------------------------------------------
// list_memories(Some(&Scope::Project(...))) — specific project only
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_scope_filter_project_specific() {
    let (repo, _tmp) = make_repo().await;

    save(&repo, "global-mem", Scope::Root).await;
    save(&repo, "proj-mem", path("test-proj")).await;
    save(&repo, "other-proj-mem", path("other-proj")).await;

    let memories = repo
        .list_memories(Some(&path("test-proj")))
        .await
        .expect("list should succeed");

    assert_eq!(memories.len(), 1, "expected only the test-proj memory");
    assert_eq!(memories[0].name.as_str(), "proj-mem");
    assert_eq!(memories[0].metadata.scope, path("test-proj"));
}

// ---------------------------------------------------------------------------
// list_memories(None) — all scopes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_scope_filter_all() {
    let (repo, _tmp) = make_repo().await;

    save(&repo, "global-mem", Scope::Root).await;
    save(&repo, "proj-mem", path("test-proj")).await;

    let memories = repo.list_memories(None).await.expect("list should succeed");

    assert_eq!(memories.len(), 2, "expected both memories");
    let names: Vec<&str> = memories.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"global-mem"));
    assert!(names.contains(&"proj-mem"));
}

// ---------------------------------------------------------------------------
// Production path: two targeted list_memories calls merged (ProjectAndGlobal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_scope_filter_project_and_global() {
    let (repo, _tmp) = make_repo().await;

    save(&repo, "global-mem", Scope::Root).await;
    save(&repo, "proj-mem", path("test-proj")).await;
    save(&repo, "other-proj-mem", path("other-proj")).await;

    // Mirror the production code path in server.rs: two targeted calls merged.
    let project_scope = path("test-proj");
    let mut memories = repo
        .list_memories(Some(&Scope::Root))
        .await
        .expect("root list should succeed");
    memories.extend(
        repo.list_memories(Some(&project_scope))
            .await
            .expect("project list should succeed"),
    );

    assert_eq!(
        memories.len(),
        2,
        "expected global + test-proj memories, not other-proj"
    );
    let names: Vec<&str> = memories.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"global-mem"), "missing global-mem");
    assert!(names.contains(&"proj-mem"), "missing proj-mem");
    assert!(
        !names.contains(&"other-proj-mem"),
        "other-proj-mem should be excluded"
    );
}
