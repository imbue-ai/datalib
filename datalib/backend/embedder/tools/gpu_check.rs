//! Runs the embedder on a machine's GPU and holds it to the CPU: embeds
//! every prompt on the best device, then the first `<n_cpu>` on the CPU,
//! and fails unless the GPU was used and each pair agrees past the parity
//! test's bound. Built by `.github/workflows/cuda.yml` beside
//! `libggml-cuda.so` and run by hand on a rented GPU
//! (docs/dev/plans/embed_on_modal.md), since CI has none.
//!
//! Usage: gpu_check <prompts> <model.gguf> [n_cpu [out_dir]]
//! `<prompts>` is `<hash>\t<prompt as a JSON string>` lines, as
//! `//tests/fixtures:ingested/search_prompts.tsv` holds them. With
//! `out_dir`, the vectors go to `gpu.jsonl` and `cpu.jsonl` there, one
//! JSON array per prompt in order, to compare one machine's with another's.

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
    let (prompts, model, n_cpu, out) = match args.as_slice() {
        [_, p, m] => (p, m, 32, None),
        [_, p, m, n] => (p, m, n.parse().context("n_cpu")?, None),
        [_, p, m, n, o] => (p, m, n.parse().context("n_cpu")?, Some(Path::new(o))),
        _ => bail!("usage: gpu_check <prompts> <model.gguf> [n_cpu [out_dir]]"),
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
    report("gpu", prompts.len(), tokens.iter().sum(), gpu_secs);

    let some = &prompts[..n_cpu.min(prompts.len())];
    let mut cpu = Embedder::open(model, Device::Cpu, 0)?;
    let (cpu_vectors, cpu_secs, cpu_tokens) = embed_all(&mut cpu, some)?;
    drop(cpu);
    report("cpu", some.len(), cpu_tokens.iter().sum(), cpu_secs);
    if let Some(dir) = out {
        write_jsonl(&dir.join("gpu.jsonl"), &gpu_vectors)?;
        write_jsonl(&dir.join("cpu.jsonl"), &cpu_vectors)?;
    }

    let cosines: Vec<f64> = cpu_vectors
        .iter()
        .zip(&gpu_vectors)
        .map(|(c, g)| cosine(c, g))
        .collect();
    let mut order: Vec<usize> = (0..cosines.len()).collect();
    order.sort_by(|&a, &b| cosines[a].total_cmp(&cosines[b]));
    for &i in order.iter().take(5) {
        println!("prompt {i}: cosine {:.6}, {} tokens", cosines[i], tokens[i]);
    }
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

/// The vectors, the seconds they took, and how many tokens each took.
type Embedded = (Vec<Vec<f32>>, f64, Vec<usize>);

fn embed_all(e: &mut Embedder, prompts: &[String]) -> Result<Embedded> {
    let start = Instant::now();
    let mut tokens = Vec::with_capacity(prompts.len());
    let mut out = Vec::with_capacity(prompts.len());
    for p in prompts {
        tokens.push(e.tokenize(p)?.len().min(e.context()));
        out.push(e.embed_fitted(p)?.0);
    }
    Ok((out, start.elapsed().as_secs_f64(), tokens))
}

fn write_jsonl(path: &Path, vectors: &[Vec<f32>]) -> Result<()> {
    let mut body = String::new();
    for v in vectors {
        body.push_str(&serde_json::to_string(v)?);
        body.push('\n');
    }
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))
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
