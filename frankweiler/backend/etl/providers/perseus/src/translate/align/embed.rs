//! Ancient-Greek-BERT sentence embedder via candle-transformers.
//!
//! Loads `pranaydeeps/Ancient-Greek-BERT` (BERT-base, continued-
//! pretrained from mBERT on Ancient Greek — the experiment showed
//! this is the best off-the-shelf option for grc-eng cross-lingual
//! similarity, beating mBERT, XLM-R, and pure-Greek GreBerta).
//!
//! On first use, weights / tokenizer / config are pulled from
//! HuggingFace into the standard HF Hub cache under `~/.cache/
//! huggingface/hub/` via `hf-hub`. Subsequent runs read from cache.
//!
//! Embeddings are mean-pooled over real (non-padding) tokens and
//! L2-normalized, so cosine similarity reduces to a dot product —
//! the form `dp::align` expects.
//!
//! Bit-equivalence with the Python reference verified on the
//! Thucydides opening: candle and HF transformers produce embeddings
//! that agree to four decimal places on identical input.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::tokio::Api;
use tokenizers::Tokenizer;

/// HuggingFace model ID. Pulls the `main` branch tip — `pranaydeeps/
/// Ancient-Greek-BERT` is effectively dormant upstream (last update
/// 2021), so silent-rewrite risk is small in practice. SHA-pinning
/// via `Repo::with_revision` is desirable on principle but tickles
/// a bug in `hf-hub` 0.3's redirect handler (HuggingFace returns a
/// relative `Location` header for commit-pinned URLs and reqwest
/// rejects the follow-up with "relative URL without a base"). Revisit
/// when this crate bumps to `hf-hub` 0.4, which restructures the
/// metadata path and is expected to fix this. Last known good
/// commit SHA: `5e3e29ece1d63029baa226f11105b1e8277c4f07`.
pub const MODEL_ID: &str = "pranaydeeps/Ancient-Greek-BERT";

#[derive(Clone)]
pub struct Embedder {
    inner: Arc<EmbedderInner>,
}

struct EmbedderInner {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Fetch (or hit cache) the model + tokenizer + config and
    /// construct an Embedder ready to take sentences.
    pub async fn load() -> Result<Self> {
        let device = Device::Cpu;
        let api = Api::new().context("init hf-hub Api")?;
        let repo = api.model(MODEL_ID.to_string());

        let config_path = fetch(&repo, "config.json").await?;
        let tokenizer_path = fetch(&repo, "tokenizer.json").await?;
        let weights_path = match repo.get("model.safetensors").await {
            Ok(p) => p,
            Err(_) => repo
                .get("pytorch_model.bin")
                .await
                .context("fetch weights (safetensors or pytorch_model.bin)")?,
        };

        let config: Config =
            serde_json::from_slice(&std::fs::read(&config_path).context("read config.json")?)
                .context("parse config.json")?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;

        let vb = if weights_path.extension().and_then(|s| s.to_str()) == Some("safetensors") {
            // Safety: mmap of an immutable on-disk file; standard
            // candle pattern. The lifetime of the mapping is tied to
            // VarBuilder which the BertModel keeps alive.
            unsafe {
                VarBuilder::from_mmaped_safetensors(
                    std::slice::from_ref(&weights_path),
                    DTYPE,
                    &device,
                )?
            }
        } else {
            VarBuilder::from_pth(&weights_path, DTYPE, &device)?
        };
        let model = BertModel::load(vb, &config).context("BertModel::load")?;

        Ok(Self {
            inner: Arc::new(EmbedderInner {
                model,
                tokenizer,
                device,
            }),
        })
    }

    /// Encode a sentence to a mean-pooled, L2-normalized embedding.
    /// Returns an owned `Vec<f32>` of length `config.hidden_size`.
    pub fn embed_one(&self, sentence: &str) -> Result<Vec<f32>> {
        let i = &self.inner;
        let enc = i
            .tokenizer
            .encode(sentence, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let ids = Tensor::new(enc.get_ids(), &i.device)?.unsqueeze(0)?;
        let type_ids = Tensor::new(enc.get_type_ids(), &i.device)?.unsqueeze(0)?;
        let mask = Tensor::new(enc.get_attention_mask(), &i.device)?.unsqueeze(0)?;
        let mask_f = mask.to_dtype(DType::F32)?;

        let hidden = i.model.forward(&ids, &type_ids, Some(&mask))?; // (1, T, D)
        let h = hidden.squeeze(0)?; // (T, D)
        let m = mask_f.squeeze(0)?.unsqueeze(1)?; // (T, 1)
        let summed = h.broadcast_mul(&m)?.sum(0)?; // (D,)
        let denom = mask_f.sum(1)?.squeeze(0)?.to_scalar::<f32>()?.max(1e-6);
        let pooled = (summed / denom as f64)?;
        let norm = pooled
            .sqr()?
            .sum_all()?
            .sqrt()?
            .to_scalar::<f32>()?
            .max(1e-9);
        let pooled = (pooled / norm as f64)?;
        let v: Vec<f32> = pooled.to_vec1()?;
        Ok(v)
    }
}

async fn fetch(repo: &hf_hub::api::tokio::ApiRepo, file: &str) -> Result<PathBuf> {
    repo.get(file)
        .await
        .with_context(|| format!("hf-hub fetch {MODEL_ID}/{file}"))
}
