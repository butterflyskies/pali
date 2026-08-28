//! Shared construction for Pali subprocess tests.

/// Construct a Pali command without ambient runtime configuration.
///
/// Tests for ordinary subprocess behavior must not depend on whether the test
/// runner has old or current Pali variables configured. Each test adds its
/// explicit fixtures after calling this helper.
pub fn pali_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_pali"));
    for (name, _) in std::env::vars_os() {
        let canonical_name = name.to_string_lossy().to_ascii_uppercase();
        if canonical_name.starts_with("MEMORY_MCP_") || canonical_name.starts_with("PALI_") {
            command.env_remove(name);
        }
    }
    command
}
