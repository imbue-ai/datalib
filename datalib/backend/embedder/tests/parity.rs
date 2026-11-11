//! The embedder against qmd 2.8.3 itself, on made-up texts: the string each
//! shows the model, the tokens, and the vector. qmd runs as it shipped,
//! through `tests/qmd_embed.mjs` on the pinned node and package, loading
//! the same pinned model file.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use datalib_embedder::{format, Device, Embedder};
use serde_json::{json, Value};

fn runfile(var: &str) -> PathBuf {
    let rel = std::env::var(var).unwrap_or_else(|_| panic!("{var} unset: is it on the test rule?"));
    let p = runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

/// (title, text): the shapes a rendered document has, and the ones a
/// tokenizer gets wrong: leading and repeated spaces, tabs, newlines,
/// accents, CJK, emoji, markup, and text that spells a special token.
fn docs() -> Vec<(Option<&'static str>, String)> {
    let long = "The away team beamed down to the surface at dawn. ".repeat(60);
    vec![
        (Some("Planning"), "lunch on friday".into()),
        (None, "  two leading spaces,  and   runs\tof\twhitespace\n\nnew paragraph".into()),
        (Some("Café"), "naïve façade — résumé, Straße, Ångström".into()),
        (Some("会議"), "明日の会議は十時からです。よろしくお願いします。".into()),
        (None, "launch 🚀 status: ✅ done, 🙂 thanks".into()),
        (
            Some("Thread"),
            "<div id=\"m-1\" data-section-uuid=\"1\" class=\"msg\">\n**Worf:** _today_ is a [good day](https://example.com)\n</div>".into(),
        ),
        (None, "the literal text <eos> and <bos> and <start_of_turn> is just text".into()),
        (Some("Log"), long),
    ]
}

fn queries() -> Vec<&'static str> {
    vec!["warp core breach", "who said today is a good day", "会議"]
}

struct Side {
    formatted: String,
    tokens: Vec<i64>,
    vector: Vec<f32>,
}

fn parse(v: &Value) -> Vec<Side> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|d| Side {
            formatted: d["formatted"].as_str().unwrap().to_string(),
            tokens: d["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_i64().unwrap())
                .collect(),
            vector: d["vector"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap() as f32)
                .collect(),
        })
        .collect()
}

/// qmd's answers for every text, once per process: (docs, queries).
fn qmd(cpu_only: bool) -> (Vec<Side>, Vec<Side>) {
    let input = json!({
        "docs": docs().iter().map(|(title, text)| json!({"text": text, "title": title})).collect::<Vec<_>>(),
        "queries": queries(),
    });
    let home = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(runfile("NODE_RLOC"));
    cmd.arg(runfile("QMD_SCRIPT_RLOC"))
        .arg(runfile("QMD_PACKAGE_RLOC"))
        .arg(runfile("EMBED_MODEL_RLOC"))
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path())
        .env("QMD_EMBED_PARALLELISM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if cpu_only {
        cmd.env("QMD_FORCE_CPU", "1");
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "qmd_embed.mjs failed: {:?}",
        out.status
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    (parse(&v["docs"]), parse(&v["queries"]))
}

fn qmd_cpu() -> &'static (Vec<Side>, Vec<Side>) {
    static ONCE: OnceLock<(Vec<Side>, Vec<Side>)> = OnceLock::new();
    ONCE.get_or_init(|| qmd(true))
}

/// An embedder of the test's own, dropped when it ends: on Metal, ggml
/// aborts at exit if a model is still loaded, so none may live in a static.
fn open(device: Device) -> Embedder {
    Embedder::open(&runfile("EMBED_MODEL_RLOC"), device, 2).unwrap()
}

fn ours_formatted() -> (Vec<String>, Vec<String>) {
    (
        docs()
            .iter()
            .map(|(title, text)| format::document(text, *title))
            .collect(),
        queries().iter().map(|q| format::query(q)).collect(),
    )
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b))
}

#[test]
fn the_prompts_are_qmds_string_for_string() {
    let (docs, queries) = qmd_cpu();
    let (our_docs, our_queries) = ours_formatted();
    let theirs: Vec<&str> = docs.iter().map(|d| d.formatted.as_str()).collect();
    assert_eq!(our_docs, theirs);
    let theirs: Vec<&str> = queries.iter().map(|d| d.formatted.as_str()).collect();
    assert_eq!(our_queries, theirs);
}

/// qmd's tokenizer call leaves out BOS and EOS, and node-llama-cpp adds
/// them before it embeds; ours adds them in the tokenizer.
#[test]
fn the_tokens_are_qmds_with_bos_and_eos() {
    let (docs, queries) = qmd_cpu();
    let embedder = open(Device::Cpu);
    for side in docs.iter().chain(queries) {
        let mut expected = vec![2i64];
        expected.extend(&side.tokens);
        expected.push(1);
        let got: Vec<i64> = embedder
            .tokenize(&side.formatted)
            .unwrap()
            .into_iter()
            .map(i64::from)
            .collect();
        assert_eq!(got, expected, "{:?}", side.formatted);
    }
}

/// node-llama-cpp's default batch for qmd's embedding context
/// (`getDefaultContextBatchSize`: the context, at most 512 tokens).
const QMD_BATCH: usize = 512;

/// How closely ours and qmd's agree on the CPU. On arm64 both run ggml's
/// NEON kernels and agree to rounding. On x86 node-llama-cpp's prebuilt
/// picks AVX2 kernels at run time, while our build keeps to the portable
/// ones so it runs on any x86, and the two round differently (0.99975 on
/// CI's runner): far closer than anything that would move a ranking.
const CPU_AGREEMENT: f64 = if cfg!(target_arch = "aarch64") {
    0.9999
} else {
    0.999
};

/// On the same backend as qmd's CPU path, and evaluated in qmd's batches,
/// the vectors agree to within `CPU_AGREEMENT`.
#[test]
fn the_vectors_match_qmd_on_the_cpu() {
    let (docs, queries) = qmd_cpu();
    let mut embedder = open(Device::Cpu);
    for side in docs.iter().chain(queries) {
        let tokens = embedder.tokenize(&side.formatted).unwrap();
        let got = embedder.embed_in_batches(&tokens, QMD_BATCH).unwrap();
        assert_eq!(got.len(), side.vector.len());
        let c = cosine(&got, &side.vector);
        assert!(c > CPU_AGREEMENT, "cosine {c} for {:?}", side.formatted);
    }
}

/// qmd evaluated an input over 512 tokens in pieces, and the pooled vector
/// it kept is the last piece's: the earlier tokens never saw the later
/// ones. Every qmd chunk over 512 tokens, most of its 900-token chunks,
/// was embedded so. Ours takes the whole input at once, so where an input
/// fits one batch the two agree, and past it they part.
#[test]
fn qmd_embedded_an_input_over_512_tokens_from_its_last_batch_only() {
    let (docs, queries) = qmd_cpu();
    let mut embedder = open(Device::Cpu);
    let mut long = 0;
    for side in docs.iter().chain(queries) {
        let tokens = embedder.tokenize(&side.formatted).unwrap();
        let whole = cosine(&embedder.embed_tokens(&tokens).unwrap(), &side.vector);
        if tokens.len() <= QMD_BATCH {
            assert!(
                whole > CPU_AGREEMENT,
                "cosine {whole} for {:?}",
                side.formatted
            );
        } else {
            long += 1;
            assert!(
                whole < 0.99,
                "cosine {whole} for a {}-token input",
                tokens.len()
            );
        }
    }
    assert!(long > 0, "no test input is over {QMD_BATCH} tokens");
}

/// What qmd wrote into a real index ran on the GPU where there was one
/// (Metal on a mac). Ours on its best device, evaluated in qmd's batches,
/// agrees with it closely.
#[test]
fn the_vectors_match_qmd_on_its_default_backend() {
    let (docs, queries) = qmd(false);
    let mut embedder = open(Device::Best);
    for side in docs.iter().chain(&queries) {
        let tokens = embedder.tokenize(&side.formatted).unwrap();
        let got = embedder.embed_in_batches(&tokens, QMD_BATCH).unwrap();
        let c = cosine(&got, &side.vector);
        assert!(c > 0.999, "cosine {c} for {:?}", side.formatted);
    }
}

/// A mac's build has Metal, and the best device on one is its GPU.
#[cfg(target_os = "macos")]
#[test]
fn a_mac_runs_the_model_on_metal() {
    assert!(datalib_embedder::gpu_available());
    assert!(open(Device::Best).device().starts_with("MTL"));
    assert_eq!(open(Device::Cpu).device(), "CPU");
}

/// The GPU's vectors and the CPU's agree, whole inputs included.
#[test]
fn the_best_device_agrees_with_the_cpu() {
    let mut gpu = open(Device::Best);
    let mut cpu = open(Device::Cpu);
    for (title, text) in docs() {
        let text = format::document(&text, title);
        let c = cosine(&gpu.embed(&text).unwrap(), &cpu.embed(&text).unwrap());
        assert!(c > 0.999, "cosine {c} for {text:?}");
    }
}

/// The rate on 3,600-byte inputs, qmd's chunk budget, on every core. Not a
/// check: run it with `--test_arg=--ignored --test_output=all`.
#[test]
#[ignore]
fn measure_rate() {
    let text = "The away team beamed down to the surface at dawn, and the tricorder readings were unusual. "
        .repeat(40);
    let text = format::document(&text[..3600], Some("Log"));
    let threads = std::thread::available_parallelism().unwrap().get() as u32;
    for device in [Device::Cpu, Device::Best] {
        let mut embedder = Embedder::open(&runfile("EMBED_MODEL_RLOC"), device, threads).unwrap();
        let tokens = embedder.tokenize(&text).unwrap();
        embedder.embed_tokens(&tokens).unwrap();
        let n = 40;
        let started = std::time::Instant::now();
        for _ in 0..n {
            embedder.embed_tokens(&tokens).unwrap();
        }
        let secs = started.elapsed().as_secs_f64();
        writeln!(
            std::io::stderr(),
            "{device:?}: {} tokens per input, {threads} threads: {:.1} inputs/s",
            tokens.len(),
            n as f64 / secs
        )
        .unwrap();
    }
}
