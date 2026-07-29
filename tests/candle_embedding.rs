//! Integration tests for the candle BERT embedding pipeline.
//!
//! These tests validate the production `CandleEmbeddingEngine` end-to-end:
//! correct dimensions, normalisation, determinism, semantic similarity,
//! and batch/single consistency (attention mask correctness).

use std::time::Duration;

use pali::embedding::{CandleEmbeddingEngine, EmbeddingBackend};
use pali::health::SubsystemReporter;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm_a * norm_b)
}

/// The engine must produce 384-dimensional vectors for BGE-small-en-v1.5.
#[tokio::test]
async fn produces_384_dim_vectors() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    assert_eq!(engine.dimensions(), 384);

    let vec = engine.embed_one("hello world").await.unwrap();
    assert_eq!(vec.len(), 384);
}

/// Vectors must be L2-normalised (unit length).
#[tokio::test]
async fn vectors_are_normalised() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    let vec = engine.embed_one("test normalisation").await.unwrap();
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
}

/// Same input must produce identical output (deterministic).
#[tokio::test]
async fn self_consistency() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    let a = engine.embed_one("determinism check").await.unwrap();
    let b = engine.embed_one("determinism check").await.unwrap();
    assert_eq!(a, b);
}

/// Semantically similar texts should cluster together.
#[tokio::test]
async fn semantic_similarity() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();

    let rust = engine.embed_one("Rust programming language").await.unwrap();
    let cargo = engine
        .embed_one("cargo build system for Rust")
        .await
        .unwrap();
    let recipe = engine
        .embed_one("chocolate cake baking recipe")
        .await
        .unwrap();

    let sim_related = cosine_similarity(&rust, &cargo);
    let sim_unrelated = cosine_similarity(&rust, &recipe);

    assert!(
        sim_related > sim_unrelated,
        "related texts should be more similar: {sim_related} vs {sim_unrelated}"
    );
}

/// Batch embed must return one vector per input, all normalised.
#[tokio::test]
async fn batch_embed() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    let texts: Vec<String> = vec!["first".into(), "second".into(), "third".into()];
    let vecs = engine.embed(&texts).await.unwrap();
    assert_eq!(vecs.len(), 3);
    for v in &vecs {
        assert_eq!(v.len(), 384);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
    }
}

/// Batch and single-item embedding must produce the same vectors.
/// This validates that the attention mask correctly excludes padding tokens.
#[tokio::test]
async fn batch_single_consistency() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();

    // Deliberately different lengths to force padding in the batch path.
    let short = "hi";
    let long = "this is a much longer sentence that will require more tokens to encode properly";

    let single_short = engine.embed_one(short).await.unwrap();
    let single_long = engine.embed_one(long).await.unwrap();

    let batch = engine
        .embed(&[short.to_string(), long.to_string()])
        .await
        .unwrap();

    let sim_short = cosine_similarity(&single_short, &batch[0]);
    let sim_long = cosine_similarity(&single_long, &batch[1]);

    // With correct attention masking, batch and single should produce
    // identical vectors. Allow tiny floating-point tolerance.
    assert!(
        sim_short > 0.9999,
        "short text: batch vs single similarity too low: {sim_short}"
    );
    assert!(
        sim_long > 0.9999,
        "long text: batch vs single similarity too low: {sim_long}"
    );
}

/// Embedding an empty batch must return an empty vec (not an error).
#[tokio::test]
async fn empty_batch_returns_empty() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    let result = engine.embed(&[]).await.unwrap();
    assert!(result.is_empty());
}

/// Batches larger than MAX_BATCH_SIZE (64) must be chunked transparently.
/// Verifies that the chunking wrapper produces one vector per input and that
/// vectors are identical regardless of which chunk they land in.
#[tokio::test]
async fn large_batch_is_chunked() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();

    // 65 texts: first chunk of 64, second chunk of 1.
    let texts: Vec<String> = (0..65).map(|i| format!("sentence number {i}")).collect();
    let vecs = engine.embed(&texts).await.unwrap();
    assert_eq!(vecs.len(), 65, "must return one vector per input");

    for (i, v) in vecs.iter().enumerate() {
        assert_eq!(v.len(), 384, "vector {i} has wrong dimensions");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "vector {i} not normalised: norm = {norm}"
        );
    }

    // Index 63 is the last text in the first 64-item chunk, padded alongside
    // 63 other sequences. Compare against a standalone embed to verify the
    // attention mask prevents padding contamination in a multi-item batch.
    let single = engine.embed_one("sentence number 63").await.unwrap();
    let sim = cosine_similarity(&vecs[63], &single);
    assert!(
        sim > 0.9999,
        "chunked text differs from single embed: similarity = {sim}"
    );
}

/// Text exceeding the 512-token limit must be truncated, not rejected.
#[tokio::test]
async fn long_text_is_truncated() {
    let engine = CandleEmbeddingEngine::new(TEST_TIMEOUT, 64, SubsystemReporter::new()).unwrap();
    let long_text = "word ".repeat(600); // ~600 tokens, well over the 512 limit
    let vec = engine.embed_one(&long_text).await.unwrap();
    assert_eq!(vec.len(), 384);
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
}
