//! Ancient-Greek-BERT sentence embedder via candle-transformers.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::tokio::Api;
use hf_hub::{Repo, RepoType};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

/// HuggingFace model ID, pinned to one commit: `resolve/main/…` is a
/// moving pointer, and a re-upload would change every alignment score
/// without a diff anywhere in this tree.
pub const MODEL_ID: &str = "pranaydeeps/Ancient-Greek-BERT";
pub const MODEL_REVISION: &str = "5e3e29ece1d63029baa226f11105b1e8277c4f07";

/// sha256 of each file at [`MODEL_REVISION`]: the weights' digest is
/// HuggingFace's LFS oid for the blob (which is its sha256), the two
/// small files were hashed after download. Checked after every fetch
/// and on every load, since hf-hub's cache is just files on disk.
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
        let api = Api::new().context("init hf-hub Api")?;
        let repo = api.repo(Repo::with_revision(
            MODEL_ID.to_string(),
            RepoType::Model,
            MODEL_REVISION.to_string(),
        ));

        let config_path = fetch(&repo, "config.json").await?;
        let tokenizer_path = fetch(&repo, "tokenizer.json").await?;
        let weights_path = fetch(&repo, "model.safetensors").await?;

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

async fn fetch(repo: &hf_hub::api::tokio::ApiRepo, file: &str) -> Result<PathBuf> {
    let path = repo
        .get(file)
        .await
        .with_context(|| format!("hf-hub fetch {MODEL_ID}@{MODEL_REVISION}/{file}"))?;
    let want = PINNED_FILES
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, sha)| *sha)
        .with_context(|| format!("{file} has no pinned sha256"))?;
    let got = sha256_file(&path)?;
    if got != want {
        bail!(
            "{} is not the pinned {MODEL_ID}@{MODEL_REVISION}/{file}: sha256 {got}, expected {want}",
            path.display()
        );
    }
    Ok(path)
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
