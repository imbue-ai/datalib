//! The embedder against qmd 2.8.3, whose vectors search was built on, on
//! made-up texts: the string each shows the model, the tokens, and the
//! vector. qmd's answers are `tests/qmd_cpu_golden.json`, captured from
//! qmd itself on its CPU path before it left the tree; its `made_by` names
//! the commit whose `tests/qmd_embed.mjs` made it. Never remake it with
//! this embedder: the test would then check the embedder against itself.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use datalib_embedder::{Device, Embedder};
use datalib_search_index::prompt as format;
use serde_json::Value;

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

/// qmd's answers for every text: (docs, queries), in the order of
/// [`docs`] and [`queries`].
fn qmd_cpu() -> &'static (Vec<Side>, Vec<Side>) {
    static ONCE: OnceLock<(Vec<Side>, Vec<Side>)> = OnceLock::new();
    ONCE.get_or_init(|| {
        let golden = std::fs::read(runfile("QMD_GOLDEN_RLOC")).unwrap();
        let v: Value = serde_json::from_slice(&golden).unwrap();
        let (docs, queries) = (parse(&v["docs"]), parse(&v["queries"]));
        assert_eq!(
            (docs.len(), queries.len()),
            (self::docs().len(), self::queries().len()),
            "the golden answers every text"
        );
        (docs, queries)
    })
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

/// How closely ours and qmd's agree on the CPU. The golden was made on
/// arm64, and there both run ggml's NEON kernels and agree to rounding.
/// On x86 our build keeps to ggml's portable kernels so it runs on any
/// x86, and they round differently (qmd's own AVX2 run agreed to 0.99975
/// on CI's runner): far closer than anything that would move a ranking.
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
