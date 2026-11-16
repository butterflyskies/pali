//! The deterministic test embedder and the populated-store self-check.
//!
//! The earlier stub embedder returned no vectors, so every `remember` failed
//! and the store stayed empty; any "nothing leaked" assertion against it
//! could not fail. These tests pin the replacement embedder's contract and
//! prove the self-check both passes on a working harness and reports a
//! broken one as broken.

mod common;
// This crate uses only part of the harness; the rest would warn as dead code.
#[allow(dead_code)]
#[path = "common/mcp_harness.rs"]
mod mcp_harness;

use async_trait::async_trait;
use mcp_harness::{
    assert_populated_store, populated_store_self_check, SelfCheckStage, StoreSeed, TestServer,
    ToolReply,
};
use pali::{embedding::EmbeddingBackend, error::MemoryError, health::HealthRegistry};

const SCOPE_A: &str = "principal-a";

fn loopback_hosts() -> Vec<String> {
    vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ]
}

fn healthy_registry() -> HealthRegistry {
    let registry = HealthRegistry::new();
    registry.git.report_ok();
    registry.embedding.report_ok();
    registry.vector_index.report_ok();
    registry
}

/// The old stub's behaviour: succeeds but returns no vectors at all.
struct NoVectorsEmbedding;

#[async_trait]
impl EmbeddingBackend for NoVectorsEmbedding {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(vec![])
    }

    fn dimensions(&self) -> usize {
        common::HASH_EMBEDDING_DIMS
    }
}

/// A degenerate embedder: every text gets the same valid unit vector, so
/// whatever is stored is always the nearest neighbour.
struct ConstantEmbedding;

#[async_trait]
impl EmbeddingBackend for ConstantEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let value = (common::HASH_EMBEDDING_DIMS as f32).sqrt().recip();
        Ok(vec![vec![value; common::HASH_EMBEDDING_DIMS]; texts.len()])
    }

    fn dimensions(&self) -> usize {
        common::HASH_EMBEDDING_DIMS
    }
}

/// Embedders that ignore their input, driven by a seeded xorshift64 stream
/// so each control run is reproducible rather than passing or failing by
/// luck.
enum Noise {
    /// Every component uniform in [-1, 1): unrelated vectors on every call.
    Random,
    /// The constant vector plus uniform jitter of at most 1e-3 per component.
    ConstantJitter,
}

struct SeededEmbedding {
    noise: Noise,
    state: std::sync::Mutex<u64>,
}

impl SeededEmbedding {
    fn new(noise: Noise, seed: u64) -> Self {
        Self {
            noise,
            // xorshift64 must not start at zero.
            state: std::sync::Mutex::new(seed.max(1)),
        }
    }

    /// Uniform in [-1, 1).
    fn next_unit(&self) -> f32 {
        let mut state = self.state.lock().expect("embedding RNG lock");
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn vector(&self) -> Vec<f32> {
        let base = (common::HASH_EMBEDDING_DIMS as f32).sqrt().recip();
        let mut vector: Vec<f32> = (0..common::HASH_EMBEDDING_DIMS)
            .map(|_| match self.noise {
                Noise::Random => self.next_unit(),
                Noise::ConstantJitter => base + 1e-3 * self.next_unit(),
            })
            .collect();
        let norm = cosine(&vector, &vector).sqrt();
        vector.iter_mut().for_each(|x| *x /= norm);
        vector
    }
}

#[async_trait]
impl EmbeddingBackend for SeededEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(texts.iter().map(|_| self.vector()).collect())
    }

    fn dimensions(&self) -> usize {
        common::HASH_EMBEDDING_DIMS
    }
}

/// Run the self-check once per seed on a fresh server and require every run
/// to report the harness broken at the recall step.
async fn assert_broken_for_every_seed(noise: fn() -> Noise, label: &str) {
    const SEEDS: u64 = 64;
    for seed in 1..=SEEDS {
        let tmp = tempfile::tempdir().expect("temp dir");
        let state = common::build_state_with_embedding(
            &tmp,
            healthy_registry(),
            Box::new(SeededEmbedding::new(noise(), seed)),
        );
        let client = TestServer::new(common::build_test_router(state, loopback_hosts()))
            .connect()
            .await
            .expect("connect");
        let outcome = populated_store_self_check(&client, &StoreSeed::canary(SCOPE_A)).await;
        // Each iteration builds a server; closing the session releases it.
        client.close().await.expect("close session");
        let Err(broken) = outcome else {
            panic!("{label} embedder passed the self-check with seed {seed}: {outcome:?}");
        };
        assert_eq!(
            broken.stage,
            SelfCheckStage::Recall,
            "seed {seed}: {broken}"
        );
        assert!(
            broken.detail.contains("does not separate"),
            "seed {seed}: {}",
            broken.detail
        );
        assert!(broken.to_string().starts_with("HARNESS BROKEN"));
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// Red if the embedder drops inputs (the #377 stub returned zero vectors for
// one input), changes between calls, or stops normalising.
#[tokio::test]
async fn hash_embedder_returns_one_stable_unit_vector_per_input() {
    let embedder = common::StubEmbeddingBackend;
    let texts = vec![
        "amber heron lantern".to_string(),
        "Amber, HERON; lantern!".to_string(),
        String::new(),
    ];

    let first = embedder.embed(&texts).await.expect("embed");
    let second = embedder.embed(&texts).await.expect("embed");

    assert_eq!(first.len(), texts.len(), "one vector per input");
    assert_eq!(first, second, "same input, same vectors");
    assert_eq!(
        first[0], first[1],
        "case and punctuation do not change tokens"
    );
    for vector in &first {
        assert_eq!(vector.len(), embedder.dimensions());
        let norm = cosine(vector, vector).sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit length, got {norm}");
    }
}

// Red if hashing stops placing shared tokens in shared buckets, which would
// leave recall unable to rank a seeded memory above unrelated text.
#[tokio::test]
async fn hash_embedder_scores_shared_tokens_above_unrelated_text() {
    let embedder = common::StubEmbeddingBackend;
    let texts = vec![
        "populated store canary amber heron lantern".to_string(),
        "canary amber heron".to_string(),
        "quarterly invoice reconciliation spreadsheet".to_string(),
    ];
    let vectors = embedder.embed(&texts).await.expect("embed");

    let related = cosine(&vectors[0], &vectors[1]);
    let unrelated = cosine(&vectors[0], &vectors[2]);
    assert!(
        related > unrelated,
        "related {related} should exceed unrelated {unrelated}"
    );
}

// Red if the default harness cannot populate the store: the condition under
// which every isolation result is vacuous.
#[tokio::test]
async fn self_check_passes_on_default_harness() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let router = common::build_test_router(common::build_healthy_state(&tmp), loopback_hosts());
    let client = TestServer::new(router).connect().await.expect("connect");
    let seed = StoreSeed::canary(SCOPE_A);

    let receipt = assert_populated_store(&client, &seed).await;

    assert!(receipt.list_count() >= 2, "{receipt:?}");
    assert_eq!(receipt.scope(), SCOPE_A);

    // The seed is readable by name too, so later assertions can rely on it.
    let read = client
        .call_tool(
            "read",
            serde_json::json!({ "name": seed.name(), "scope": seed.scope() }),
        )
        .await
        .expect("read");
    let ToolReply::Success(text) = read else {
        panic!("read did not succeed: {read:?}");
    };
    assert!(text.contains("amber heron lantern"), "{text}");
}

// Control: the self-check must be able to fail. With the probe's embedder,
// `remember` fails, so the check reports the harness broken at the seed step.
#[tokio::test]
async fn self_check_reports_broken_when_embedder_returns_no_vectors() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state =
        common::build_state_with_embedding(&tmp, healthy_registry(), Box::new(NoVectorsEmbedding));
    let router = common::build_test_router(state, loopback_hosts());
    let client = TestServer::new(router).connect().await.expect("connect");

    let broken = populated_store_self_check(&client, &StoreSeed::canary(SCOPE_A))
        .await
        .expect_err("an empty store must not pass the self-check");

    assert_eq!(broken.stage, SelfCheckStage::Seed);
    assert!(
        broken.detail.contains("embedding returned no vectors"),
        "{}",
        broken.detail
    );
    let message = broken.to_string();
    assert!(message.starts_with("HARNESS BROKEN"), "{message}");
    assert!(message.contains("not as a pass"), "{message}");
}

// Control: a degenerate embedder stores and lists fine, and keyword search
// still ranks each memory first for its own text, so only the semantic
// distance comparison can catch it. Red if the check stops comparing them.
#[tokio::test]
async fn self_check_reports_broken_when_embedder_is_constant() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state =
        common::build_state_with_embedding(&tmp, healthy_registry(), Box::new(ConstantEmbedding));
    let router = common::build_test_router(state, loopback_hosts());
    let client = TestServer::new(router).connect().await.expect("connect");

    let broken = populated_store_self_check(&client, &StoreSeed::canary(SCOPE_A))
        .await
        .expect_err("a constant embedder must not pass the self-check");

    assert_eq!(broken.stage, SelfCheckStage::Recall, "{broken}");
    assert!(
        broken.detail.contains("does not separate"),
        "{}",
        broken.detail
    );
    assert!(broken.to_string().starts_with("HARNESS BROKEN"), "{broken}");
}

// Control: an embedder returning unrelated random vectors on every call.
// Without the near-zero own-distance bound, two coin flips on random
// distances passed about 1 run in 4 (certification review of #403).
#[tokio::test]
async fn self_check_reports_broken_for_random_embedder() {
    assert_broken_for_every_seed(|| Noise::Random, "random").await;
}

// Control: a constant embedder with 1e-3 jitter keeps every distance near
// zero, so only the separation margin can reject it; a bare `t < o` passed
// about 1 run in 10.
#[tokio::test]
async fn self_check_reports_broken_for_jittered_constant_embedder() {
    assert_broken_for_every_seed(|| Noise::ConstantJitter, "constant+jitter").await;
}

// Control: a keyword index that cannot serve. Recall then drops to
// embedding-only results; the check must refuse rather than pass, because a
// leak through keyword search could not show. The repair slot is held so the
// self-healing rebuild cannot race the check and restore the index.
#[tokio::test]
async fn self_check_reports_broken_when_keyword_index_is_down() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = common::build_healthy_state(&tmp);
    state
        .lexical
        .mark_rebuild_required("populated-store control");
    assert!(state.lexical.try_claim_repair(), "repair slot must be free");
    let client = TestServer::new(common::build_test_router(state, loopback_hosts()))
        .connect()
        .await
        .expect("connect");

    let broken = populated_store_self_check(&client, &StoreSeed::canary(SCOPE_A))
        .await
        .expect_err("a dead keyword index must not pass the self-check");

    assert_eq!(broken.stage, SelfCheckStage::Recall, "{broken}");
    assert!(
        broken.detail.contains("only as \"semantic\""),
        "{}",
        broken.detail
    );
}

// Control for the asserting wrapper: a broken harness panics as BROKEN, so a
// test built on it can never report the run as a pass.
#[tokio::test]
#[should_panic(expected = "HARNESS BROKEN: populated-store self-check failed at Seed")]
async fn assert_populated_store_panics_as_broken_on_empty_store() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state =
        common::build_state_with_embedding(&tmp, healthy_registry(), Box::new(NoVectorsEmbedding));
    let router = common::build_test_router(state, loopback_hosts());
    let client = TestServer::new(router).connect().await.expect("connect");

    let _receipt = assert_populated_store(&client, &StoreSeed::canary(SCOPE_A)).await;
}

// Red if the client folds a JSON-RPC error into success or a transport
// failure; isolation tests must tell a server-side refusal from both.
#[tokio::test]
async fn client_classifies_unknown_tool_as_rpc_error() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let router = common::build_test_router(common::build_healthy_state(&tmp), loopback_hosts());
    let client = TestServer::new(router).connect().await.expect("connect");

    let reply = client
        .call_tool("no-such-tool", serde_json::json!({}))
        .await
        .expect("the server answers");

    let ToolReply::RpcError { code, message } = reply else {
        panic!("expected a JSON-RPC error, got {reply:?}");
    };
    assert_ne!(code, 0, "{message}");
    assert!(!message.is_empty());
}

/// Two independent servers, each with its own store.
fn two_servers() -> (tempfile::TempDir, TestServer, tempfile::TempDir, TestServer) {
    let tmp_x = tempfile::tempdir().expect("temp dir");
    let tmp_y = tempfile::tempdir().expect("temp dir");
    let x = TestServer::new(common::build_test_router(
        common::build_healthy_state(&tmp_x),
        loopback_hosts(),
    ));
    let y = TestServer::new(common::build_test_router(
        common::build_healthy_state(&tmp_y),
        loopback_hosts(),
    ));
    (tmp_x, x, tmp_y, y)
}

// Red if a receipt stops being bound to its server: a receipt from X must
// cover every client on X (any principal) and no client on Y, whose store
// the check never populated.
#[tokio::test]
async fn receipt_covers_clients_on_its_server_only() {
    let (_tmp_x, x, _tmp_y, y) = two_servers();
    let client_x = x.connect().await.expect("connect");
    let receipt = assert_populated_store(&client_x, &StoreSeed::canary(SCOPE_A)).await;

    let mut other_principal = http::HeaderMap::new();
    other_principal.insert("x-test-principal", http::HeaderValue::from_static("b"));
    let second_client_x = x
        .connect_with_headers(other_principal)
        .await
        .expect("connect");
    let client_y = y.connect().await.expect("connect");

    assert!(receipt.covers(&client_x));
    assert!(receipt.covers(&second_client_x));
    assert!(!receipt.covers(&client_y));
    receipt.assert_covers(&second_client_x);
}

// Control for the asserting form: a receipt from another server is reported
// as a broken harness, never silently accepted.
#[tokio::test]
#[should_panic(expected = "HARNESS BROKEN: the self-check receipt is for test server")]
async fn assert_covers_panics_as_broken_for_another_server() {
    let (_tmp_x, x, _tmp_y, y) = two_servers();
    let client_x = x.connect().await.expect("connect");
    let receipt = assert_populated_store(&client_x, &StoreSeed::canary(SCOPE_A)).await;
    let client_y = y.connect().await.expect("connect");

    receipt.assert_covers(&client_y);
}
