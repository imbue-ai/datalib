//! Runs the embedder on a machine's GPU and holds it to the CPU: embeds
//! every prompt on the best device, once for each batching asked for, then
//! the first `--cpu` prompts on the CPU one at a time, and fails unless
//! the GPU was used and each pair agrees past [`AGREEMENT`]. Built by
//! `.github/workflows/cuda.yml` beside `libggml-cuda.so` and run by hand
//! on a rented GPU (docs/dev/plans/embed_on_modal.md), since CI has none.
//!
//! Usage: gpu_check <prompts> <model.gguf> [--cpu N] [--out DIR]
//!                  [--batching PROMPTS:TOKENS]...
//! `<prompts>` is `<hash>\t<prompt as a JSON string>` lines, as
//! `//tests/fixtures:ingested/search_prompts.tsv` holds them. Without
//! `--batching`, the device's default. With `--out`, the vectors go to
//! `cpu.jsonl` and `gpu-<prompts>x<tokens>.jsonl` there, one JSON array
//! per prompt in order, to compare one machine's with another's.

// A command-line tool: its report is its stdout.
#![allow(clippy::disallowed_macros)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use datalib_embedder::{cuda_module_report, device_for, Batching, Device, Embedder};

/// The GPU and the CPU round differently. Over the TNG fixture's prompts,
/// CUDA against the CPU is 0.9983 on its worst, a 26-token prompt, and
/// 0.9993 or better on every other; Metal against the CPU is 0.9993.
const AGREEMENT: f64 = 0.998;

struct Args {
    prompts: PathBuf,
    model: PathBuf,
    cpu: usize,
    out: Option<PathBuf>,
    batchings: Vec<Batching>,
}

fn parse_args() -> Result<Args> {
    let usage = "usage: gpu_check <prompts> <model.gguf> [--cpu N] [--out DIR] \
                 [--batching PROMPTS:TOKENS]...";
    let mut positional = Vec::new();
    let mut cpu = 32;
    let mut out = None;
    let mut batchings = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .with_context(|| format!("{arg} takes a value; {usage}"))
        };
        match arg.as_str() {
            "--cpu" => cpu = value()?.parse().context("--cpu")?,
            "--out" => out = Some(PathBuf::from(value()?)),
            "--batching" => {
                let v = value()?;
                let (p, t) = v.split_once(':').context("--batching PROMPTS:TOKENS")?;
                batchings.push(Batching {
                    prompts: p.parse().context("--batching prompts")?,
                    tokens: t.parse().context("--batching tokens")?,
                });
            }
            _ if arg.starts_with("--") => bail!("{arg}? {usage}"),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let [prompts, model] = <[PathBuf; 2]>::try_from(positional).ok().context(usage)?;
    Ok(Args {
        prompts,
        model,
        cpu,
        out,
        batchings,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let text = std::fs::read_to_string(&args.prompts)
        .with_context(|| format!("read {}", args.prompts.display()))?;
    let prompts = text
        .lines()
        .map(|line| {
            let (_, p) = line.split_once('\t').context("a line with no tab")?;
            Ok(serde_json::from_str::<String>(p)?)
        })
        .collect::<Result<Vec<_>>>()?;

    let device = device_for(Device::Best);
    println!(
        "cuda module: {}",
        cuda_module_report().unwrap_or("not looked for")
    );
    println!("device: {device}");
    ensure!(device != "CPU", "the best device is the CPU");
    let batchings = if args.batchings.is_empty() {
        vec![Batching::for_device(&device)]
    } else {
        args.batchings
    };

    let some = &prompts[..args.cpu.min(prompts.len())];
    let mut cpu = Embedder::open_with(&args.model, Device::Cpu, 0, Batching::ONE)?;
    let (cpu_vectors, cpu_secs, tokens) = embed_all(&mut cpu, some)?;
    drop(cpu);
    report(
        "cpu",
        Batching::ONE,
        some.len(),
        tokens.iter().sum(),
        cpu_secs,
    );
    if let Some(dir) = &args.out {
        write_jsonl(&dir.join("cpu.jsonl"), &cpu_vectors)?;
    }

    let mut worst_of_all = f64::INFINITY;
    for batching in batchings {
        let mut gpu = Embedder::open_with(&args.model, Device::Best, 0, batching)?;
        let (gpu_vectors, gpu_secs, gpu_tokens) = embed_all(&mut gpu, &prompts)?;
        let took = gpu.batching();
        drop(gpu);
        report(
            &device,
            took,
            prompts.len(),
            gpu_tokens.iter().sum(),
            gpu_secs,
        );
        if let Some(dir) = &args.out {
            let name = format!("gpu-{}x{}.jsonl", took.prompts, took.tokens);
            write_jsonl(&dir.join(name), &gpu_vectors)?;
        }
        worst_of_all = worst_of_all.min(agreement(&cpu_vectors, &gpu_vectors, &tokens));
    }
    ensure!(
        worst_of_all > AGREEMENT,
        "worst cosine {worst_of_all} is under {AGREEMENT}"
    );
    Ok(())
}

/// Prints how the GPU's vectors agree with the CPU's; the worst cosine.
fn agreement(cpu: &[Vec<f32>], gpu: &[Vec<f32>], tokens: &[usize]) -> f64 {
    let cosines: Vec<f64> = cpu.iter().zip(gpu).map(|(c, g)| cosine(c, g)).collect();
    let mut order: Vec<usize> = (0..cosines.len()).collect();
    order.sort_by(|&a, &b| cosines[a].total_cmp(&cosines[b]));
    for &i in order.iter().take(3) {
        println!(
            "  prompt {i}: cosine {:.6}, {} tokens",
            cosines[i], tokens[i]
        );
    }
    let worst = cosines.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = cosines.iter().sum::<f64>() / cosines.len() as f64;
    println!(
        "  against the cpu over {}: worst cosine {worst:.6}, mean {mean:.6}",
        cosines.len()
    );
    worst
}

/// The vectors, the seconds they took, and how many tokens each took.
type Embedded = (Vec<Vec<f32>>, f64, Vec<usize>);

fn embed_all(e: &mut Embedder, prompts: &[String]) -> Result<Embedded> {
    let tokens = prompts
        .iter()
        .map(|p| Ok(e.tokenize(p)?.len().min(e.context())))
        .collect::<Result<Vec<_>>>()?;
    // The first call loads kernels and buffers; it is not the rate.
    e.embed_fitted_many(&prompts[..1])?;
    let start = Instant::now();
    let out = e.embed_fitted_many(prompts)?;
    let secs = start.elapsed().as_secs_f64();
    Ok((out.into_iter().map(|(v, _)| v).collect(), secs, tokens))
}

fn write_jsonl(path: &Path, vectors: &[Vec<f32>]) -> Result<()> {
    let mut body = String::new();
    for v in vectors {
        body.push_str(&serde_json::to_string(v)?);
        body.push('\n');
    }
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

fn report(device: &str, b: Batching, n: usize, tokens: usize, secs: f64) {
    println!(
        "{device}, {} prompts / {} tokens per call: {n} prompts, {tokens} tokens in \
         {secs:.2}s: {:.1} prompts/s, {:.0} tokens/s",
        b.prompts,
        b.tokens,
        n as f64 / secs,
        tokens as f64 / secs
    );
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b))
}
