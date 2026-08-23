//! Semantic memory engine with pure-Rust embeddings and git-backed storage.
//!
//! This crate provides the library core for `pali`, an MCP server that
//! stores and retrieves memories using vector similarity search. Embeddings
//! are computed on-device via candle (BERT inference) with no C/C++ FFI.

#![warn(missing_docs)]

/// Token resolution, OAuth device flow, and credential storage backends.
pub mod auth;
/// Configuration file parsing for per-scope remote mapping.
pub mod config;

/// Embedding backends for computing vector representations of text.
pub mod embedding;
/// Error types used throughout the crate.
pub mod error;
/// Federated, custody-preserving reads across configured sibling Pali stores.
pub mod federation;
/// Filesystem utilities — atomic writes, path helpers, crash-safe temp-file-then-rename.
pub mod fs_util;
/// HTTP health-check handlers (`/readyz`).
pub mod health;
/// HNSW vector index for approximate nearest-neighbour search.
pub mod index;
/// Append-only SQLite event log for recall telemetry and threshold calibration.
pub mod recall_log;
/// Git-backed memory repository — read, write, sync, and diff operations.
pub mod repo;
/// Routes memory operations to scope-specific git repositories.
pub mod repo_router;
/// Hybrid retrieval — semantic + BM25 lexical search merged via rank fusion.
pub mod search;
/// MCP server implementation — tool handlers for the memory protocol.
pub mod server;
/// Test-only tracing capture helpers shared across unit-test modules.
#[cfg(test)]
pub(crate) mod test_log;
/// Domain types: memories, scopes, metadata, validation, and application state.
pub mod types;
