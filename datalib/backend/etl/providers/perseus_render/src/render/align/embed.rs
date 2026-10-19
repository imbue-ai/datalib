//! Ancient-Greek-BERT sentence embedder via candle-transformers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::Tokenizer;

/// HuggingFace model ID, pinned to one commit: `resolve/main/…` is a
/// moving pointer, and a re-upload would change every alignment score
/// without a diff anywhere in this tree.
pub const MODEL_ID: &str = "pranaydeeps/Ancient-Greek-BERT";
pub const MODEL_REVISION: &str = "5e3e29ece1d63029baa226f11105b1e8277c4f07";

/// sha256 of each file at [`MODEL_REVISION`]: the weights' digest is
/// HuggingFace's LFS oid for the blob (which is its sha256), the two
/// small files were hashed after download. Checked on every load: a copy
/// that hashes wrong is fetched again, and a fetch that hashes wrong is
/// refused (`datalib_fetch::download_verified`).
const PINNED_FILES: &[(&str, &str)] = &[
    (
        "config.json",
        "257eaf6fc45aa72a3e0b19dc257d2557f37718524424eab217a83acb8a28bfeb",
    ),
    (
        "tokenizer.json",
        "67cf6361f5ffb48cc2068a82be13b63abbba1b7f84f226b1083257a7befe0a49",
    ),
    (
        "model.safetensors",
        "380c4da303a7a9fdac71c16f166d67633bcb258dfb0cf9c0a94e4af9bcdd0fc1",
    ),
];

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
    pub async fn load() -> Result<Self> {
        let device = Device::Cpu;
        let dir = model_dir()?;
        let [config_path, tokenizer_path, weights_path] =
            tokio::task::spawn_blocking(move || -> Result<[PathBuf; 3]> {
                Ok([
                    fetch(&dir, "config.json")?,
                    fetch(&dir, "tokenizer.json")?,
                    fetch(&dir, "model.safetensors")?,
                ])
            })
            .await
            .context("join the model fetch")??;

        let config: Config =
            serde_json::from_slice(&std::fs::read(&config_path).context("read config.json")?)
                .context("parse config.json")?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;

        // Safety: mmap of an immutable on-disk file; standard candle
        // pattern. The lifetime of the mapping is tied to VarBuilder
        // which the BertModel keeps alive.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                std::slice::from_ref(&weights_path),
                DTYPE,
                &device,
            )?
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

/// `models/<MODEL_ID>/<MODEL_REVISION>/` in the machine cache, shared by
/// every data root on this host.
fn model_dir() -> Result<PathBuf> {
    let cache = datalib_runtime::cache::cache_dir()
        .context("neither $XDG_CACHE_HOME nor $HOME is set: nowhere to keep the model")?;
    Ok(cache.join("models").join(MODEL_ID).join(MODEL_REVISION))
}

fn fetch(dir: &Path, file: &str) -> Result<PathBuf> {
    let want = PINNED_FILES
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, sha)| *sha)
        .with_context(|| format!("{file} has no pinned sha256"))?;
    let path = dir.join(file);
    if path.is_file() && datalib_fetch::sha256_file(&path)? == want {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let url = format!("https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{file}");
    datalib_fetch::download_verified("[perseus-model]", &url, want, &path)
        .with_context(|| format!("fetch {MODEL_ID}@{MODEL_REVISION}/{file}"))?;
    Ok(path)
}
