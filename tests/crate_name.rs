//! Smoke test: the crate is importable as `pali`, not the old `memory_mcp` name.
//!
//! This test exists to catch regressions if someone re-introduces `[lib] name =
//! "memory_mcp"` or otherwise aliases the old crate name back in.

use pali::types::{Memory, MemoryMetadata, Scope};

#[test]
fn crate_is_importable_as_pali() {
    // If this compiles, the crate name is `pali`. Construct a value from each
    // key type to prove the re-exports are live, not just syntax.
    let scope = Scope::Root;
    let meta = MemoryMetadata::new(scope, vec![], None);
    let memory = Memory::new("smoke-test", "the crate formerly known as memory_mcp", meta)
        .expect("valid memory");
    let _ = memory;
}
