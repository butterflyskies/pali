//! Filesystem utilities — atomic writes, crash-safe temp-file-then-rename,
//! and path helpers.

use std::path::{Path, PathBuf};

use crate::error::MemoryError;

/// Expand a leading `~` to the user's home directory.
///
/// - `~/foo` → `$HOME/foo`
/// - `~` alone → `$HOME`
/// - `~user/...` → error (not supported)
/// - Absolute or relative paths pass through unchanged.
pub fn expand_tilde(path: &str) -> Result<PathBuf, MemoryError> {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = dirs::home_dir().ok_or_else(|| {
                MemoryError::Internal("could not expand '~': home directory unknown".into())
            })?;
            Ok(home.join(rest.strip_prefix('/').unwrap_or(rest)))
        }
        Some(_) => Err(MemoryError::Internal(
            "~user expansion is not supported; use an absolute path or ~/...".into(),
        )),
        None => Ok(PathBuf::from(path)),
    }
}

/// Resolve a path to a canonical absolute form, tolerating missing suffixes.
///
/// `std::fs::canonicalize` fails when the path does not exist yet, but repo
/// paths are frequently created only by the init that follows a collision
/// check. This canonicalizes the deepest **existing** ancestor of the
/// unnormalized path — letting the OS resolve symlinks and `..` together —
/// then appends the genuinely missing components, so two spellings of the
/// same physical location resolve to the same value (#293 review, rounds
/// 4–6).
///
/// A `..` that touches anything *existing* is never stripped lexically: in
/// `link/../x` where `link` is a symlink, the `..` resolves against the
/// symlink target, so only the filesystem may interpret it (#293 review,
/// round 5). A `..` inside the proven-missing suffix, however, *is*
/// normalized lexically: nothing in that suffix exists, so it cannot
/// contain a symlink, and the `..` can only cancel the missing component
/// spelled before it — exactly what `create_dir_all` followed by kernel
/// path resolution would have produced once the components existed. This
/// keeps previously valid spellings like `existing/missing/../repo`
/// working (#293 review, round 6; ADR-0041 compatibility).
///
/// "Missing" means **proven absent**: at each step of the upward walk a
/// component joins the missing suffix only if `symlink_metadata` fails
/// with `NotFound`. A component that *exists* but cannot be resolved — a
/// regular file where a directory is needed, a dangling symlink, a
/// permission-denied directory — is an error preserving the underlying
/// [`std::io::ErrorKind`], never silently reclassified as missing (#293
/// review, round 7: `file/../repo` used to fail with `ENOTDIR` at
/// `create_dir_all`; treating the unresolvable prefix as missing would
/// lexically cancel the `..` and silently redirect to sibling `repo`).
///
/// A `..` that leads the missing suffix climbs into the canonicalized
/// ancestor instead. That is also resolved lexically — safe because the
/// ancestor is already fully resolved, so its lexical parent is its
/// physical parent. A `..` that would climb above the filesystem root is
/// an error.
///
/// Public because the server binary must canonicalize the default repo path
/// with the exact same rules *before* opening it, so the opened location and
/// the router's collision-detection key can never diverge.
pub fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, MemoryError> {
    /// One component of the not-yet-existing suffix, recorded during the
    /// upward walk and replayed onto the canonicalized ancestor.
    enum Tail {
        /// A named component to re-append.
        Normal(std::ffi::OsString),
        /// A `..` proven to sit on a non-existent prefix; cancels one
        /// component lexically during rebuild.
        Parent,
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| {
                MemoryError::Internal(format!("cannot resolve current working directory: {e}"))
            })?
            .join(path)
    };

    // Walk up to the deepest existing ancestor of the *unnormalized* path,
    // canonicalize it, then re-append the missing tail. `..` components in
    // the existing portion are resolved by `canonicalize` itself; `.` is
    // identity and is normalized away by `file_name`/`parent`.
    let mut base = absolute.as_path();
    let mut tail: Vec<Tail> = Vec::new();
    let canonical_base = loop {
        match base.canonicalize() {
            Ok(resolved) => break resolved,
            Err(canonicalize_err) => {
                // `canonicalize` failing does NOT prove `base` is missing
                // (#293 review, round 7): it also fails when the path
                // *exists* but cannot resolve — a regular file in
                // directory position, a dangling symlink, a
                // permission-denied directory. Classifying those as
                // missing would lexically cancel a later `..` against a
                // component the filesystem would have interpreted
                // differently (or refused), silently redirecting to a
                // sibling path. Only a `NotFound` from `symlink_metadata`
                // proves absence; anything else fails closed, preserving
                // the underlying error kind.
                match std::fs::symlink_metadata(base) {
                    Ok(_) => {
                        // The component itself exists (lstat succeeded)
                        // yet cannot be canonicalized — e.g. a dangling
                        // symlink whose target is gone.
                        return Err(MemoryError::Io(std::io::Error::new(
                            canonicalize_err.kind(),
                            format!(
                                "path component '{}' exists but cannot be resolved \
                                 (while canonicalizing '{}'): {canonicalize_err}",
                                base.display(),
                                absolute.display(),
                            ),
                        )));
                    }
                    Err(meta_err) if meta_err.kind() == std::io::ErrorKind::NotFound => {
                        // Proven absent — record the component below and
                        // keep walking upward.
                    }
                    Err(meta_err) => {
                        // Exists-but-untraversable prefix: a regular file
                        // where a directory is needed (`NotADirectory`),
                        // permission denied, etc.
                        return Err(MemoryError::Io(std::io::Error::new(
                            meta_err.kind(),
                            format!(
                                "cannot access path component '{}' \
                                 (while canonicalizing '{}'): {meta_err}",
                                base.display(),
                                absolute.display(),
                            ),
                        )));
                    }
                }
                match (base.file_name(), base.parent()) {
                    (Some(name), Some(parent)) => {
                        tail.push(Tail::Normal(name.to_os_string()));
                        base = parent;
                    }
                    // `file_name()` returns `None` when `base` terminates
                    // in `..` (a bare root always canonicalizes, so it
                    // cannot reach the `Err` arm). The metadata check
                    // above proved this `..`-terminated path names
                    // nothing: its prefix is absent (a file or dangling
                    // symlink in the prefix errors out instead), so
                    // there is no symlink to reinterpret the `..` and it
                    // is safe to cancel lexically during rebuild.
                    (None, Some(parent)) => {
                        tail.push(Tail::Parent);
                        base = parent;
                    }
                    _ => {
                        return Err(MemoryError::InvalidInput {
                            reason: format!(
                                "cannot resolve any existing ancestor of path '{}'",
                                absolute.display()
                            ),
                        });
                    }
                }
            }
        }
    };

    // Replay the missing suffix onto the canonicalized ancestor. `Parent`
    // cancels either the missing component pushed just before it or — when
    // the `..` leads the suffix — one component of the canonicalized
    // ancestor, whose lexical parent is its physical parent.
    let mut resolved = canonical_base;
    for component in tail.iter().rev() {
        match component {
            Tail::Normal(name) => resolved.push(name),
            Tail::Parent => {
                if !resolved.pop() {
                    return Err(MemoryError::InvalidInput {
                        reason: format!(
                            "path '{}' uses '..' to climb above the filesystem root",
                            absolute.display()
                        ),
                    });
                }
            }
        }
    }
    Ok(resolved)
}

/// RAII guard that removes a temp file on drop unless defused.
struct TempGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl<'a> TempGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, active: true }
    }

    /// Disarm the guard so the temp file is **not** deleted on drop.
    fn defuse(&mut self) {
        self.active = false;
    }
}

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Atomically write `data` to `path` via a temp file and rename.
///
/// 1. Creates a temp file (`.tmp`) in the same directory as `path`.
/// 2. On Unix the temp file is opened with mode `0o600`.
/// 3. Writes `data`, calls `sync_all`, then renames into `path`.
/// 4. Fsyncs the parent directory so the rename is durable on crash.
/// 5. If any step after temp-file creation fails, the temp file is
///    cleaned up automatically (RAII guard).
///
/// Callers must ensure no concurrent writes target the same `path`.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;

    // Build a deterministic temp name: .<filename>.tmp
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?
        .to_string_lossy();
    let tmp_path = parent.join(format!(".{file_name}.tmp"));

    let mut guard = TempGuard::new(&tmp_path);

    // Open + write + sync.
    write_tmp(&tmp_path, data)?;

    // Atomic rename into the final location.
    std::fs::rename(&tmp_path, path)?;
    guard.defuse();

    // Fsync the parent directory so the rename is durable even on hard crash.
    // Best-effort: the rename already committed, so a dir-fsync failure should
    // not cause callers to treat the write as failed.
    if let Err(e) = fsync_dir(parent) {
        tracing::warn!("fsync of parent directory failed (data is written): {e}");
    }

    Ok(())
}

/// Create and write the temp file with platform-appropriate options.
fn write_tmp(tmp_path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(tmp_path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

/// Fsync a directory to ensure metadata (renames) is persisted.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let d = std::fs::File::open(dir)?;
    d.sync_all()?;
    Ok(())
}

/// No-op on non-Unix — Windows flushes directory metadata on rename.
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("test.txt");

        atomic_write(&target, b"hello world").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "hello world");
        // Temp file should not linger.
        assert!(!dir.path().join(".test.txt.tmp").exists());
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("test.txt");

        fs::write(&target, "old content").unwrap();
        atomic_write(&target, b"new content").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new content");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.txt");

        atomic_write(&target, b"secret").unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file should be 0600, got {mode:o}");
    }

    #[test]
    fn temp_file_cleaned_on_missing_parent() {
        // Writing to a path whose parent doesn't exist should fail
        // and not leave debris.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nonexistent_dir").join("file.txt");

        assert!(atomic_write(&target, b"data").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn temp_file_cleaned_on_rename_failure() {
        // Make the rename target a *directory* so write_tmp succeeds
        // (creating .file.txt.tmp in the parent) but rename fails with
        // EISDIR. This exercises TempGuard cleanup after a real write.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        let tmp = dir.path().join(".file.txt.tmp");

        // Create a directory at the target path — rename(file, dir) → EISDIR.
        fs::create_dir(&target).unwrap();

        let result = atomic_write(&target, b"data");
        assert!(result.is_err(), "expected rename to fail with EISDIR");
        // TempGuard should have cleaned up the temp file.
        assert!(!tmp.exists(), "temp file should be cleaned up by TempGuard");
        // The directory target should still be intact.
        assert!(target.is_dir(), "target directory should be untouched");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_tightens_permissions_on_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wide.txt");

        // Create file with wide permissions.
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        // Overwrite via atomic_write — should end up 0o600.
        atomic_write(&target, b"new").unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "overwritten file should be 0600, got {mode:o}");
    }

    // -- canonicalize_allow_missing (#293 review, round 4) -----------------
    //
    // Repo-path collision detection depends on distinct spellings of the
    // same physical location resolving identically, including paths whose
    // tail does not exist yet.

    #[test]
    fn canonicalize_allow_missing_resolves_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().canonicalize().unwrap();
        let missing = dir.path().join("not-yet/created");
        let resolved = canonicalize_allow_missing(&missing).unwrap();
        assert_eq!(resolved, existing.join("not-yet/created"));
    }

    #[test]
    fn canonicalize_allow_missing_resolves_existing_dot_segments() {
        // `..` over an *existing* directory is resolved by the filesystem,
        // and a missing tail may still follow it.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        let spelled = dir.path().join("a/../b/./c");
        let plain = dir.path().join("b/c");
        assert_eq!(
            canonicalize_allow_missing(&spelled).unwrap(),
            canonicalize_allow_missing(&plain).unwrap()
        );
    }

    #[test]
    fn canonicalize_allow_missing_normalizes_dot_dot_in_missing_tail() {
        // ADR-0041 compatibility (#293 review, round 6): before
        // canonicalization was applied at startup, `existing/missing/../repo`
        // worked — `create_dir_all` created `missing`, the kernel walked the
        // `..` back out, and the repo landed at `existing/repo`. The missing
        // suffix contains no symlinks (nothing in it exists), so the `..`
        // cancels its preceding missing component lexically instead of
        // aborting.
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().canonicalize().unwrap();
        let spelled = dir.path().join("missing/../repo");
        assert_eq!(
            canonicalize_allow_missing(&spelled).unwrap(),
            existing.join("repo")
        );
    }

    #[test]
    fn canonicalize_allow_missing_resolves_leading_dot_dot_against_ancestor() {
        // A `..` that leads the missing suffix climbs into the canonicalized
        // ancestor. The ancestor is fully resolved, so its lexical parent is
        // its physical parent — `dir/missing/../../repo` → parent-of-dir/repo.
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().canonicalize().unwrap();
        let spelled = dir.path().join("missing/../../repo");
        assert_eq!(
            canonicalize_allow_missing(&spelled).unwrap(),
            existing.parent().unwrap().join("repo")
        );
    }

    #[test]
    fn canonicalize_allow_missing_rejects_dot_dot_above_root() {
        // Enough `..` in a missing suffix to climb above `/` cannot name any
        // location; it must error, not wrap around or silently clamp.
        let spelled = PathBuf::from("/pali-test-nonexistent-4f2a9c/../../still-missing/repo");
        let err = canonicalize_allow_missing(&spelled).unwrap_err();
        assert!(
            err.to_string().contains("root"),
            "error must name the climb above root: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_allow_missing_resolves_dot_dot_against_symlink_target() {
        // Syne's round-5 repro: base/link -> else/dir. On a real filesystem
        // base/link/../repo traverses to else/repo (`..` resolves against
        // the symlink TARGET); a lexical normalization would answer
        // base/repo, letting a mapped route at else/repo alias the same
        // physical repo without tripping collision detection.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let else_dir = tmp.path().join("else");
        fs::create_dir_all(else_dir.join("dir")).unwrap();
        fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(else_dir.join("dir"), base.join("link")).unwrap();

        let via_symlink = canonicalize_allow_missing(&base.join("link/../repo")).unwrap();
        let direct = canonicalize_allow_missing(&else_dir.join("repo")).unwrap();
        assert_eq!(
            via_symlink, direct,
            "`..` after a symlink must resolve against the target"
        );

        let lexical = canonicalize_allow_missing(&base.join("repo")).unwrap();
        assert_ne!(
            via_symlink, lexical,
            "must not produce the lexical answer base/repo"
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_allow_missing_resolves_symlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        // Both an existing symlink and a missing tail beneath it resolve to
        // the physical location.
        assert_eq!(
            canonicalize_allow_missing(&alias).unwrap(),
            canonicalize_allow_missing(&real).unwrap()
        );
        assert_eq!(
            canonicalize_allow_missing(&alias.join("sub")).unwrap(),
            canonicalize_allow_missing(&real.join("sub")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_file_in_prefix_fails_closed_not_redirected() {
        // Syne's round-7 repro: `file/../repo1` where `file` is an existing
        // regular file. Before startup canonicalization, `create_dir_all`
        // failed with ENOTDIR; classifying the unresolvable prefix as
        // "missing" would lexically cancel the `..` and silently redirect
        // to sibling `repo1`. The helper must fail closed instead,
        // preserving the underlying error kind.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), b"plain file").unwrap();

        let spelled = dir.path().join("file/../repo1");
        let err = canonicalize_allow_missing(&spelled).unwrap_err();
        match &err {
            MemoryError::Io(io_err) => assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::NotADirectory,
                "must preserve the underlying error kind: {io_err}"
            ),
            other => panic!("expected MemoryError::Io, got: {other}"),
        }
        assert!(
            err.to_string().contains("cannot access path component"),
            "error must name the unresolvable component: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_in_prefix_fails_closed_not_redirected() {
        // Syne's round-7 repro: `broken/../repo2` where `broken` is a
        // dangling symlink. Traversal used to fail at open time; treating
        // the existing-but-unresolvable symlink as missing would return
        // sibling `repo2` — a silent redirect that startup would then open
        // as a different (possibly empty) repo. The helper must fail
        // closed on the existing symlink instead.
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("broken")).unwrap();

        let spelled = dir.path().join("broken/../repo2");
        let err = canonicalize_allow_missing(&spelled).unwrap_err();
        match &err {
            MemoryError::Io(io_err) => assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::NotFound,
                "canonicalize of a dangling symlink fails NotFound: {io_err}"
            ),
            other => panic!("expected MemoryError::Io, got: {other}"),
        }
        assert!(
            err.to_string().contains("exists but cannot be resolved"),
            "error must state the component exists but is unresolvable: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_rejects_symlink_temp_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        let tmp = dir.path().join(".file.txt.tmp");

        // Pre-plant a symlink at the temp path.
        let decoy = dir.path().join("decoy.txt");
        std::os::unix::fs::symlink(&decoy, &tmp).unwrap();

        // atomic_write should fail because O_NOFOLLOW rejects the symlink.
        let result = atomic_write(&target, b"secret");
        assert!(result.is_err(), "should reject symlink at temp path");

        // Decoy should not have been written to.
        assert!(!decoy.exists(), "symlink target should be untouched");
    }
}
