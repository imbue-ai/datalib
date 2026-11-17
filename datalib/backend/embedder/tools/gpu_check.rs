//! Runs the embedder on a machine's GPU and holds it to the CPU: embeds
//! every prompt on the best device, then the first `<n_cpu>` on the CPU,
//! and fails unless the GPU was used and each pair agrees past the parity
//! test's bound. Built by `.github/workflows/cuda.yml` beside
//! `libggml-cuda.so` and run by hand on a rented GPU
//! (docs/dev/plans/embed_on_modal.md), since CI has none.
//!
//! Usage: gpu_check <prompts> <model.gguf> [n_cpu]
//! `<prompts>` is `<hash>\t<prompt as a JSON string>` lines, as
//! `//tests/fixtures:ingested/search_prompts.tsv` holds them.

// A command-line tool: its report is its stdout.
#![allow(clippy::disallowed_macros)]

use std::path::Path;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use datalib_embedder::{cuda_module_report, Device, Embedder};

/// The GPU and the CPU round differently; Metal against the CPU clears
/// this in `tests/parity.rs`.
const AGREEMENT: f64 = 0.999;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (prompts, model, n_cpu) = match args.as_slice() {
        [_, p, m] => (p, m, 32),
        [_, p, m, n] => (p, m, n.parse().context("n_cpu")?),
        _ => bail!("usage: gpu_check <prompts> <model.gguf> [n_cpu]"),
    };
    let text = std::fs::read_to_string(prompts).with_context(|| format!("read {prompts}"))?;
    let prompts = text
        .lines()
        .map(|line| {
            let (_, p) = line.split_once('\t').context("a line with no tab")?;
            Ok(serde_json::from_str::<String>(p)?)
        })
        .collect::<Result<Vec<_>>>()?;
    let model = Path::new(model);

    let mut gpu = Embedder::open(model, Device::Best, 0)?;
    println!(
        "cuda module: {}",
        cuda_module_report().unwrap_or("not looked for")
    );
    println!("device: {}", gpu.device());
    ensure!(gpu.device() != "CPU", "the best device is the CPU");
    let (gpu_vectors, gpu_secs, tokens) = embed_all(&mut gpu, &prompts)?;
    drop(gpu);
    report("gpu", prompts.len(), tokens, gpu_secs);

    let some = &prompts[..n_cpu.min(prompts.len())];
    let mut cpu = Embedder::open(model, Device::Cpu, 0)?;
    let (cpu_vectors, cpu_secs, cpu_tokens) = embed_all(&mut cpu, some)?;
    drop(cpu);
    report("cpu", some.len(), cpu_tokens, cpu_secs);

    let cosines: Vec<f64> = cpu_vectors
        .iter()
        .zip(&gpu_vectors)
        .map(|(c, g)| cosine(c, g))
        .collect();
    let worst = cosines.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = cosines.iter().sum::<f64>() / cosines.len() as f64;
    println!(
        "gpu against cpu over {}: worst cosine {worst:.6}, mean {mean:.6}",
        cosines.len()
    );
    ensure!(
        worst > AGREEMENT,
        "worst cosine {worst} is under {AGREEMENT}"
    );
    Ok(())
}

/// The vectors, the seconds they took, and how many tokens went in.
fn embed_all(e: &mut Embedder, prompts: &[String]) -> Result<(Vec<Vec<f32>>, f64, usize)> {
    let start = Instant::now();
    let mut tokens = 0;
    let mut out = Vec::with_capacity(prompts.len());
    for p in prompts {
        tokens += e.tokenize(p)?.len().min(e.context());
        out.push(e.embed_fitted(p)?.0);
    }
    Ok((out, start.elapsed().as_secs_f64(), tokens))
}

fn report(device: &str, n: usize, tokens: usize, secs: f64) {
    println!(
        "{device}: {n} prompts, {tokens} tokens in {secs:.2}s: {:.1} prompts/s, {:.0} tokens/s",
        n as f64 / secs,
        tokens as f64 / secs
    );
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b))
}
