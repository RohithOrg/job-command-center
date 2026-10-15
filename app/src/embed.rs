//! Built-in, fully offline sentence embeddings (pure Rust, CPU only).
//!
//! Model: sentence-transformers/all-MiniLM-L6-v2 (Apache-2.0), 384 dims,
//! mean pooling over the last hidden state followed by L2 normalisation —
//! identical to the reference `SentenceTransformer(...).encode(normalize_embeddings=True)`.
//!
//! The tokenizer, config and (f16) weights are compiled into the binary with
//! `include_bytes!`, so nothing is downloaded or read from disk at runtime.
//! Weights are stored as f16 to halve their size and upcast to f32 on load
//! (CPU matmul in f32 is fast and accurate).
//!
//! Inference runs on candle (CPU backend); tokenisation uses the `tokenizers`
//! crate with the pure-Rust `fancy-regex` backend (no onig / C deps).
//! NOTE: candle is pinned to 0.9.x on purpose; candle-core 0.10+ hard-depends on
//! `tokenizers` with the `onig` feature, which compiles the Oniguruma C library.
//!
//! `embed` is synchronous and thread-safe; call it from `spawn_blocking`.
//! The model is loaded lazily on first use (~100 ms) and shared afterwards.

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection,
    TruncationParams, TruncationStrategy,
};

/// Identifier stored alongside vectors so embeddings from different models never mix.
pub const MODEL_ID: &str = "builtin:all-MiniLM-L6-v2";
/// Output dimensionality.
#[cfg(test)]
pub const DIMS: usize = 384;

/// Max tokens per text (incl. [CLS]/[SEP]); matches the model's `max_seq_length`.
const MAX_TOKENS: usize = 256;
/// Texts per forward pass. Inputs are length-sorted first, so padding stays small.
const BATCH_SIZE: usize = 8;
/// Upper bound on batches run concurrently. candle parallelises matmuls but runs
/// element-wise ops (softmax, GELU, layer-norm, ...) on one thread, so overlapping
/// a few batches roughly doubles throughput on a multi-core CPU.
const MAX_WORKERS: usize = 4;

static TOKENIZER_JSON: &[u8] = include_bytes!("../models/all-MiniLM-L6-v2/tokenizer.json");
static CONFIG_JSON: &[u8] = include_bytes!("../models/all-MiniLM-L6-v2/config.json");
static WEIGHTS: &[u8] = include_bytes!("../models/all-MiniLM-L6-v2/model.f16.safetensors");

struct Embedder {
    tokenizer: Tokenizer,
    model: BertModel,
    device: Device,
}

/// (input index, vector) pairs produced by one worker.
type Indexed = Vec<(usize, Vec<f32>)>;

static EMBEDDER: OnceLock<std::result::Result<Embedder, String>> = OnceLock::new();

fn embedder() -> Result<&'static Embedder> {
    EMBEDDER
        .get_or_init(|| Embedder::load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("failed to load built-in embedding model: {e}"))
}

impl Embedder {
    fn load() -> Result<Self> {
        let device = Device::Cpu;

        let mut tokenizer = Tokenizer::from_bytes(TOKENIZER_JSON)
            .map_err(anyhow::Error::msg)
            .context("parse tokenizer.json")?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                direction: TruncationDirection::Right,
                max_length: MAX_TOKENS,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
            }))
            .map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
        }));

        let config: Config = serde_json::from_slice(CONFIG_JSON).context("parse config.json")?;
        // f16 on disk -> f32 tensors in memory.
        let vb = VarBuilder::from_slice_safetensors(WEIGHTS, DType::F32, &device)
            .context("load weights")?;
        let model = BertModel::load(vb, &config).context("build BERT model")?;

        Ok(Self {
            tokenizer,
            model,
            device,
        })
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)
            .context("tokenize")?;
        let n = encodings.len();
        let seq_len = encodings.first().map(|e| e.get_ids().len()).unwrap_or(0);

        let mut ids = Vec::with_capacity(n * seq_len);
        let mut mask = Vec::with_capacity(n * seq_len);
        for enc in &encodings {
            ids.extend(enc.get_ids().iter().map(|&x| x as i64));
            mask.extend(enc.get_attention_mask().iter().map(|&x| x as i64));
        }
        let input_ids = Tensor::from_vec(ids, (n, seq_len), &self.device)?;
        let attention_mask = Tensor::from_vec(mask, (n, seq_len), &self.device)?;
        let token_type_ids = input_ids.zeros_like()?;

        // (n, seq, hidden)
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Mean pooling over non-padding tokens.
        let mask_f = attention_mask.to_dtype(DType::F32)?.unsqueeze(2)?; // (n, seq, 1)
        let summed = hidden.broadcast_mul(&mask_f)?.sum(1)?; // (n, hidden)
        let counts = mask_f.sum(1)?.clamp(1e-9f32, f32::MAX)?; // (n, 1)
        let pooled = summed.broadcast_div(&counts)?;

        // L2 normalise.
        let norms = pooled
            .sqr()?
            .sum_keepdim(1)?
            .sqrt()?
            .clamp(1e-12f32, f32::MAX)?;
        let normed = pooled.broadcast_div(&norms)?;
        Ok(normed.to_vec2::<f32>()?)
    }
}

/// Embed `texts` into L2-normalised `DIMS`-dimensional vectors (same order as input).
///
/// Texts longer than 256 tokens are truncated. Blocking and CPU heavy: call from
/// `tokio::task::spawn_blocking`. Safe to call concurrently from several threads.
pub fn embed(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let emb = embedder()?;

    // Sort by length so each batch pads to similar lengths, then restore order.
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].len());

    let batches: Vec<&[usize]> = order.chunks(BATCH_SIZE).collect();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = (cores / 2).clamp(1, MAX_WORKERS).min(batches.len());

    // Single batch: run inline, no thread spawn.
    if batches.len() == 1 {
        return emb.embed_batch(&texts.iter().map(String::as_str).collect::<Vec<_>>());
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Result<Indexed>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| -> Result<Indexed> {
                    let mut local = Vec::new();
                    loop {
                        let b = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(chunk) = batches.get(b) else { break };
                        let batch: Vec<&str> = chunk.iter().map(|&i| texts[i].as_str()).collect();
                        let vecs = emb.embed_batch(&batch)?;
                        local.extend(chunk.iter().copied().zip(vecs));
                    }
                    Ok(local)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow!("embedding worker panicked")))
            })
            .collect()
    });

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    for r in results {
        for (i, v) in r? {
            out[i] = v;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn shape_and_normalisation() {
        let texts = vec!["hello world".to_string(), String::new(), "a ".repeat(2000)];
        let v = embed(&texts).unwrap();
        assert_eq!(v.len(), 3);
        for e in &v {
            assert_eq!(e.len(), DIMS);
            let n: f32 = e.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-4, "norm {n}");
        }
        assert!(embed(&[]).unwrap().is_empty());
    }

    #[test]
    fn semantic_sanity() {
        let t = |s: &str| s.to_string();
        let v = embed(&[
            t("visa sponsorship for the Netherlands"),
            t("Needs Dutch work permit sponsorship (highly skilled migrant)"),
            t("Built Kubernetes deployment pipelines"),
        ])
        .unwrap();
        let (rel, unrel) = (cos(&v[0], &v[1]), cos(&v[0], &v[2]));
        assert!(rel > unrel + 0.1, "related {rel} vs unrelated {unrel}");
    }

    #[test]
    fn batching_is_order_preserving_and_deterministic() {
        let texts: Vec<String> = (0..40)
            .map(|i| format!("text number {i} {}", "x ".repeat(i)))
            .collect();
        let all = embed(&texts).unwrap();
        let single = embed(&texts[7..8]).unwrap();
        assert!(cos(&all[7], &single[0]) > 0.9999);
    }
}
