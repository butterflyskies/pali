use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{api::sync::ApiBuilder, Cache, Repo, RepoType};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};

use super::EmbeddingBackend;
use crate::error::MemoryError;
use crate::health::SubsystemReporter;

/// HuggingFace model ID. Only BGE-small-en-v1.5 is supported currently.
pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

struct EmbedRequest {
    texts: Vec<String>,
    enqueued_at: Instant,
    state: Arc<AtomicU8>,
    reply_tx: oneshot::Sender<Result<Vec<Vec<f32>>, MemoryError>>,
}

const REQUEST_QUEUED: u8 = 0;
const REQUEST_STARTED: u8 = 1;
const REQUEST_ABANDONED: u8 = 2;

/// Pure-Rust embedding engine using candle for BERT inference.
///
/// Uses candle-transformers' BERT implementation with tokenizers for
/// tokenisation. No C/C++ FFI dependencies — compiles on all platforms.
///
/// A dedicated OS thread owns the model exclusively. Async callers send work
/// via a channel and await a oneshot reply. If a call times out, the caller
/// gets an error immediately; the worker finishes its current task, discards
/// the stale reply channel, and picks up the next request — no restart needed.
pub struct CandleEmbeddingEngine {
    // Option so Drop can take ownership of tx to close it before joining.
    tx: Option<mpsc::SyncSender<EmbedRequest>>,
    worker: Option<std::thread::JoinHandle<()>>,
    dim: usize,
    embed_timeout: Duration,
    reporter: SubsystemReporter,
}

struct CandleInner {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl CandleEmbeddingEngine {
    /// Initialise the candle embedding engine.
    ///
    /// Downloads model weights from HuggingFace Hub on first use (cached
    /// in the standard HF cache directory, respects `HF_HOME`).
    ///
    /// `embed_timeout` caps how long a single [`embed`](Self::embed) call may
    /// block. If the worker is still running when the timeout fires, the caller
    /// gets an error but the engine recovers automatically — no restart needed.
    ///
    /// `queue_size` sets the bounded channel capacity — how many requests can
    /// queue behind the one being processed. Extra callers get an immediate
    /// "busy" error.
    ///
    /// `reporter` receives `report_ok`/`report_err` after each embed call so
    /// the `/readyz` handler can reflect the engine's operational state without
    /// active probing.
    pub fn new(
        embed_timeout: Duration,
        queue_size: usize,
        reporter: SubsystemReporter,
    ) -> Result<Self, MemoryError> {
        let device = Device::Cpu;

        let (config, mut tokenizer, weights_path) =
            load_model_files().map_err(|e| MemoryError::Embedding(e.to_string()))?;

        // Enable padding so encode_batch produces equal-length sequences.
        tokenizer.with_padding(Some(PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| MemoryError::Embedding(format!("failed to set truncation: {e}")))?;

        // SAFETY: `from_mmaped_safetensors` memory-maps the weights file. The
        // caller must ensure the file is not modified for the lifetime of the
        // resulting tensors. HuggingFace Hub writes cache files atomically and
        // never modifies them in-place, so the mapping is stable.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .map_err(|e| MemoryError::Embedding(format!("failed to load weights: {e}")))?
        };

        let model = BertModel::load(vb, &config)
            .map_err(|e| MemoryError::Embedding(format!("failed to build BERT model: {e}")))?;

        let dim = config.hidden_size;

        let (tx, rx) = mpsc::sync_channel::<EmbedRequest>(queue_size);

        let worker = std::thread::Builder::new()
            .name("embed-worker".into())
            .spawn(move || {
                let inner = CandleInner {
                    model,
                    tokenizer,
                    device,
                };
                worker_loop(inner, dim, rx);
            })
            .map_err(|e| MemoryError::Embedding(format!("failed to spawn embed worker: {e}")))?;

        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            dim,
            embed_timeout,
            reporter,
        })
    }

    /// Construct an engine backed by a caller-supplied worker sender.
    ///
    /// Bypasses model loading entirely. The caller is responsible for spawning
    /// a thread that reads from the other end of the channel. Used only in
    /// tests to exercise the channel mechanics (timeout, disconnect, busy)
    /// without needing the HuggingFace model cache.
    #[cfg(test)]
    fn with_worker(
        tx: mpsc::SyncSender<EmbedRequest>,
        dim: usize,
        embed_timeout: Duration,
    ) -> Self {
        Self {
            tx: Some(tx),
            worker: None,
            dim,
            embed_timeout,
            reporter: SubsystemReporter::new(),
        }
    }
}

impl Drop for CandleEmbeddingEngine {
    fn drop(&mut self) {
        // Close the channel first so the worker's `for ... in rx` loop exits.
        drop(self.tx.take());
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// Main loop for the dedicated embedding worker thread.
///
/// Receives `(texts, reply_tx)` pairs, runs inference, and sends the result
/// back on `reply_tx`. If the receiver was dropped (the async caller timed
/// out), the send fails silently and the loop continues — this is the
/// self-healing path.
fn worker_loop(mut inner: CandleInner, dim: usize, rx: mpsc::Receiver<EmbedRequest>) {
    for request in rx {
        let panicked = process_request(request, dim, |texts| embed_batch(&inner, texts));

        if panicked {
            inner.tokenizer.with_padding(Some(PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                ..Default::default()
            }));
            let _ = inner.tokenizer.with_truncation(Some(TruncationParams {
                max_length: 512,
                ..Default::default()
            }));
        }
    }
}

/// Process one queued request, skipping work whose caller has already left.
///
/// Returns `true` when inference panicked so the caller can restore the
/// tokenizer configuration before processing the next request.
fn process_request<F>(request: EmbedRequest, dim: usize, embed: F) -> bool
where
    F: FnOnce(&[String]) -> Result<Vec<Vec<f32>>, MemoryError>,
{
    let queue_wait_ms =
        u64::try_from(request.enqueued_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let texts = request.texts;
    let reply_tx = request.reply_tx;

    // Atomically claim the request for inference. A caller whose deadline
    // expires while it is still queued marks it abandoned; whichever side
    // wins this transition determines the typed timeout without a race between
    // checking the reply channel and starting expensive work.
    if request
        .state
        .compare_exchange(
            REQUEST_QUEUED,
            REQUEST_STARTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
        || reply_tx.is_closed()
    {
        tracing::info!(
            queue_wait_ms,
            batch_size = texts.len(),
            outcome = "abandoned",
            "embedding request abandoned before execution"
        );
        return false;
    }

    let span = tracing::debug_span!(
        "embedding.embed",
        batch_size = texts.len(),
        dimensions = dim,
        model = MODEL_ID,
    );
    let _enter = span.enter();

    let mut panicked = false;
    let inference_start = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| embed(&texts))).unwrap_or_else(|panic_payload| {
        panicked = true;
        let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic in embedding engine".to_string()
        };
        tracing::warn!(error = %msg, "embedding engine panicked — recovering");
        Err(MemoryError::Embedding(format!(
            "embedding engine panicked: {msg}"
        )))
    });
    let inference_ms = u64::try_from(inference_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        queue_wait_ms,
        inference_ms,
        outcome = if result.is_ok() { "success" } else { "error" },
        "embedding completed"
    );

    let _ = reply_tx.send(result);
    panicked
}

#[async_trait::async_trait]
impl EmbeddingBackend for CandleEmbeddingEngine {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(REQUEST_QUEUED));

        let tx = self
            .tx
            .as_ref()
            .ok_or(MemoryError::EmbeddingWorkerUnavailable {
                reason: "engine has been shut down",
            })?;

        tx.try_send(EmbedRequest {
            texts: texts.to_vec(),
            enqueued_at: Instant::now(),
            state: Arc::clone(&state),
            reply_tx,
        })
        .map_err(|e| match e {
            mpsc::TrySendError::Full(_) => MemoryError::EmbeddingWorkerBusy,
            mpsc::TrySendError::Disconnected(_) => MemoryError::EmbeddingWorkerUnavailable {
                reason: "worker exited; restart required",
            },
        })?;

        let result = match timeout(self.embed_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            // Fires if the worker drops reply_tx without sending (e.g. a
            // double-panic that escapes catch_unwind, or a panic in span setup).
            Ok(Err(_)) => Err(MemoryError::EmbeddingWorkerUnavailable {
                reason: "worker dropped the reply channel unexpectedly",
            }),
            Err(_elapsed) => match state.compare_exchange(
                REQUEST_QUEUED,
                REQUEST_ABANDONED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) | Err(REQUEST_ABANDONED) => Err(MemoryError::EmbeddingQueueTimeout {
                    timeout_secs: self.embed_timeout.as_secs_f64(),
                }),
                Err(REQUEST_STARTED) => Err(MemoryError::EmbeddingInferenceTimeout {
                    timeout_secs: self.embed_timeout.as_secs_f64(),
                }),
                Err(unexpected) => Err(MemoryError::Internal(format!(
                    "invalid embedding request state after timeout: {unexpected}"
                ))),
            },
        };

        // Report operational state passively so /readyz reflects reality without probing.
        match &result {
            Ok(_) => self.reporter.report_ok(),
            Err(_) => self.reporter.report_err("embed failed"),
        }

        result
    }

    fn dimensions(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// Model loading
// ---------------------------------------------------------------------------

/// Download (or retrieve from cache) the model files from HuggingFace Hub.
///
/// On first run (cold start), this downloads ~130 MB of model files from
/// HuggingFace Hub. Subsequent starts use the local cache (`HF_HOME`).
/// Use the `warmup` subcommand or a k8s init container to pre-populate the
/// cache and avoid blocking the first server startup.
fn load_model_files() -> anyhow::Result<(BertConfig, Tokenizer, PathBuf)> {
    let _span = tracing::info_span!("embedding.load_model", model = MODEL_ID).entered();

    let cache = Cache::from_env();
    let hf_repo = Repo::new(MODEL_ID.to_string(), RepoType::Model);

    // Check whether the heaviest file (model weights) is already cached.
    let cached = cache.repo(hf_repo.clone()).get("model.safetensors");
    if cached.is_none() {
        tracing::warn!(
            model = MODEL_ID,
            "embedding model not found in cache — downloading from HuggingFace Hub \
             (this may take a minute on first run; use `memory-mcp warmup` to pre-populate)"
        );
    } else {
        tracing::info!(model = MODEL_ID, "loading embedding model from cache");
    }

    // Respect HF_HOME and HF_ENDPOINT env vars; disable indicatif progress
    // bars since we are a headless server.
    let api = ApiBuilder::from_env().with_progress(false).build()?;
    let repo = api.repo(hf_repo);

    let start = std::time::Instant::now();
    let config_path = repo.get("config.json")?;
    let tokenizer_path = repo.get("tokenizer.json")?;
    let weights_path = repo.get("model.safetensors")?;
    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        "model files ready"
    );

    let config: BertConfig = serde_json::from_str(&std::fs::read_to_string(&config_path)?)?;
    let tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

    Ok((config, tokenizer, weights_path))
}

// ---------------------------------------------------------------------------
// Inference
// ---------------------------------------------------------------------------

/// Maximum texts per forward pass. BERT attention is O(batch × seq²) in memory;
/// capping the batch avoids OOM on large reindex operations. 64 is conservative
/// enough for CPU inference while still amortising per-batch overhead.
const MAX_BATCH_SIZE: usize = 64;

/// Embed texts through the BERT model, chunking into bounded forward passes.
///
/// Splits the input into chunks of at most [`MAX_BATCH_SIZE`] texts and runs
/// each chunk through [`embed_chunk`], concatenating the results.
fn embed_batch(inner: &CandleInner, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
    let _span = tracing::debug_span!("embedding.embed_batch", batch_size = texts.len()).entered();

    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(MAX_BATCH_SIZE) {
        results.extend(embed_chunk(inner, chunk)?);
    }
    Ok(results)
}

/// Embed a single chunk of texts through the BERT model in one forward pass.
///
/// Texts are tokenised with padding (to the longest sequence in the chunk)
/// and truncation (to 512 tokens), then passed through BERT together.
/// An attention mask ensures padding tokens do not affect the output.
/// CLS pooling extracts the first token's hidden state, which is then
/// L2-normalised to produce unit vectors.
fn embed_chunk(inner: &CandleInner, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
    let _span = tracing::debug_span!("embedding.embed_chunk", chunk_size = texts.len()).entered();
    debug_assert!(!texts.is_empty(), "embed_chunk called with empty texts");

    let encodings = inner
        .tokenizer
        .encode_batch(texts.to_vec(), true)
        .map_err(|e| MemoryError::Embedding(format!("tokenization failed: {e}")))?;

    let batch_size = encodings.len();
    let seq_len = encodings[0].get_ids().len();

    // Verify padding produced uniform sequence lengths before allocating
    // the flat token vectors. A mismatch here means the tokenizer's
    // padding config was not applied (e.g. silently reset).
    if let Some((i, enc)) = encodings
        .iter()
        .enumerate()
        .find(|(_, e)| e.get_ids().len() != seq_len)
    {
        return Err(MemoryError::Embedding(format!(
            "padding invariant violated: encoding[0] has {seq_len} tokens \
             but encoding[{i}] has {} — check tokenizer padding config",
            enc.get_ids().len(),
        )));
    }

    let all_ids: Vec<u32> = encodings
        .iter()
        .flat_map(|e| e.get_ids().to_vec())
        .collect();
    let all_type_ids: Vec<u32> = encodings
        .iter()
        .flat_map(|e| e.get_type_ids().to_vec())
        .collect();
    let all_masks: Vec<u32> = encodings
        .iter()
        .flat_map(|e| e.get_attention_mask().to_vec())
        .collect();

    let input_ids = Tensor::new(all_ids.as_slice(), &inner.device)
        .and_then(|t| t.reshape((batch_size, seq_len)))
        .map_err(|e| MemoryError::Embedding(format!("tensor creation failed: {e}")))?;

    let token_type_ids = Tensor::new(all_type_ids.as_slice(), &inner.device)
        .and_then(|t| t.reshape((batch_size, seq_len)))
        .map_err(|e| MemoryError::Embedding(format!("tensor creation failed: {e}")))?;

    let attention_mask = Tensor::new(all_masks.as_slice(), &inner.device)
        .and_then(|t| t.reshape((batch_size, seq_len)))
        .map_err(|e| MemoryError::Embedding(format!("tensor creation failed: {e}")))?;

    let embeddings = inner
        .model
        .forward(&input_ids, &token_type_ids, Some(&attention_mask))
        .map_err(|e| MemoryError::Embedding(format!("BERT forward pass failed: {e}")))?;

    // CLS pooling + L2 normalise each vector in the batch.
    let mut results = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
        let cls = embeddings
            .get(i)
            .and_then(|seq| seq.get(0))
            .map_err(|e| MemoryError::Embedding(format!("CLS extraction failed: {e}")))?;

        // L2 normalise with epsilon guard against division by zero
        // (e.g. malformed model weights producing an all-zero CLS vector).
        let norm = cls
            .sqr()
            .and_then(|s| s.sum_all())
            .and_then(|s| s.sqrt())
            .and_then(|n| n.maximum(1e-12))
            .and_then(|n| cls.broadcast_div(&n))
            .map_err(|e| MemoryError::Embedding(format!("L2 normalisation failed: {e}")))?;

        let vector: Vec<f32> = norm
            .to_vec1()
            .map_err(|e| MemoryError::Embedding(format!("tensor to vec failed: {e}")))?;

        results.push(vector);
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use super::*;

    /// Build a fake engine whose worker applies `handler` to every request.
    ///
    /// `handler` receives the texts and the reply sender; it can sleep, panic,
    /// drop the sender, or send any result — enabling controlled fault injection
    /// without touching the model loading path.
    fn fake_engine<F>(timeout: Duration, handler: F) -> CandleEmbeddingEngine
    where
        F: Fn(Vec<String>, oneshot::Sender<Result<Vec<Vec<f32>>, MemoryError>>) + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel::<EmbedRequest>(1);
        std::thread::spawn(move || {
            for request in rx {
                if request
                    .state
                    .compare_exchange(
                        REQUEST_QUEUED,
                        REQUEST_STARTED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                    || request.reply_tx.is_closed()
                {
                    continue;
                }
                handler(request.texts, request.reply_tx);
            }
        });
        CandleEmbeddingEngine::with_worker(tx, 4, timeout)
    }

    /// Worker that immediately returns a fixed-size zero vector per input.
    fn ok_handler(
        texts: Vec<String>,
        reply_tx: oneshot::Sender<Result<Vec<Vec<f32>>, MemoryError>>,
    ) {
        let vecs = texts.iter().map(|_| vec![0.0f32; 4]).collect();
        let _ = reply_tx.send(Ok(vecs));
    }

    #[tokio::test]
    async fn happy_path_returns_vectors() {
        let engine = fake_engine(Duration::from_secs(5), ok_handler);
        let result = engine
            .embed(&["hello".to_string(), "world".to_string()])
            .await;
        let vecs = result.expect("embed should succeed");
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), 4);
    }

    #[test]
    fn abandoned_queued_request_skips_inference() {
        let (reply_tx, reply_rx) = oneshot::channel();
        drop(reply_rx);
        let request = EmbedRequest {
            texts: vec!["already timed out".to_string()],
            enqueued_at: Instant::now(),
            state: Arc::new(AtomicU8::new(REQUEST_ABANDONED)),
            reply_tx,
        };
        let called = std::sync::atomic::AtomicBool::new(false);

        let panicked = process_request(request, 4, |_| {
            called.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(vec![vec![0.0; 4]])
        });

        assert!(!panicked);
        assert!(
            !called.load(std::sync::atomic::Ordering::Relaxed),
            "a request whose caller timed out before execution must not consume inference"
        );
    }

    #[tokio::test]
    async fn timeout_returns_error_and_worker_recovers() {
        // Barrier lets us prove the worker is still alive after the timeout
        // fires on the first request.
        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = Arc::clone(&barrier);

        let engine = fake_engine(Duration::from_millis(50), move |texts, reply_tx| {
            if texts[0] == "slow" {
                // Block until the test signals us to proceed (after timeout fires).
                barrier2.wait();
                // Reply arrives after the caller's receiver was dropped — send
                // fails silently, which is the self-healing path.
                let _ = reply_tx.send(Ok(vec![vec![0.0; 4]]));
                // Signal the test that we've finished processing the stale request.
                barrier2.wait();
            } else {
                ok_handler(texts, reply_tx);
            }
        });

        // First call times out.
        let err = engine
            .embed(&["slow".to_string()])
            .await
            .expect_err("slow embed should time out");
        assert!(matches!(err, MemoryError::EmbeddingInferenceTimeout { .. }));

        // Unblock the worker and wait for it to finish the stale request.
        barrier.wait();
        barrier.wait();

        // Second call should succeed — the worker recovered.
        let result = engine.embed(&["fast".to_string()]).await;
        assert!(
            result.is_ok(),
            "engine should recover after timeout: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_distinguishes_queued_request_from_active_inference() {
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let engine = Arc::new(fake_engine(
            Duration::from_millis(50),
            move |texts, reply_tx| {
                if texts[0] == "active" {
                    worker_barrier.wait();
                    worker_barrier.wait();
                }
                ok_handler(texts, reply_tx);
            },
        ));

        let active_engine = Arc::clone(&engine);
        let active =
            tokio::spawn(async move { active_engine.embed(&["active".to_string()]).await });

        // The first request has crossed the worker boundary and is blocked in
        // inference. A second request can enter the channel but cannot start.
        barrier.wait();
        let queued_err = engine
            .embed(&["queued".to_string()])
            .await
            .expect_err("queued request should time out behind active inference");
        assert!(matches!(
            queued_err,
            MemoryError::EmbeddingQueueTimeout { .. }
        ));

        barrier.wait();
        let active_err = active
            .await
            .expect("active task should not panic")
            .expect_err("active request should exceed its deadline");
        assert!(matches!(
            active_err,
            MemoryError::EmbeddingInferenceTimeout { .. }
        ));
    }

    #[tokio::test]
    async fn disconnected_worker_returns_error() {
        let (tx, rx) = mpsc::sync_channel::<EmbedRequest>(1);
        // Drop the receiver immediately — worker is "dead".
        drop(rx);
        let engine = CandleEmbeddingEngine::with_worker(tx, 4, Duration::from_secs(5));

        let err = engine
            .embed(&["anything".to_string()])
            .await
            .expect_err("disconnected worker should error");
        assert!(matches!(
            err,
            MemoryError::EmbeddingWorkerUnavailable { .. }
        ));
    }

    #[tokio::test]
    async fn busy_worker_returns_error() {
        // Channel capacity 0 is not allowed by SyncSender; use capacity 1 but
        // send two requests without the worker consuming either.
        // Easier: use a zero-sleep worker that we pre-fill the channel for.
        let (tx, rx) = mpsc::sync_channel::<EmbedRequest>(1);

        // Pre-fill the single channel slot by sending directly, bypassing embed().
        let (filler_tx, _filler_rx) = oneshot::channel::<Result<Vec<Vec<f32>>, MemoryError>>();
        tx.send(EmbedRequest {
            texts: vec!["fill".to_string()],
            enqueued_at: Instant::now(),
            state: Arc::new(AtomicU8::new(REQUEST_QUEUED)),
            reply_tx: filler_tx,
        })
        .unwrap();

        // Now embed() hits try_send on a full channel.
        let engine = CandleEmbeddingEngine::with_worker(tx, 4, Duration::from_secs(5));
        let err = engine
            .embed(&["overflow".to_string()])
            .await
            .expect_err("full channel should error");
        assert!(matches!(err, MemoryError::EmbeddingWorkerBusy));

        drop(rx); // clean up
    }
}
