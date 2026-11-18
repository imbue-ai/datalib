//! The embedding behaviour datalib depends on, one test per fact, in the
//! order `datalib/backend/embedder/README.md` states them. A llama.cpp or
//! model bump that moves a fact fails here by name: fix the fact in the
//! README, then the test, then whatever leaned on it. How ours agrees with
//! qmd is `tests/parity.rs`; what only a GPU this suite never sees can show
//! is measured by hand with `gpu_check` (README § "Measured by hand").

use std::path::PathBuf;

use datalib_embedder::{fit, pack, Batching, Device, Embedder, Tokenizer, CONTEXT_TOKENS};

fn model() -> PathBuf {
    let rel = std::env::var("EMBED_MODEL_RLOC").expect("EMBED_MODEL_RLOC unset");
    let p = runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{rel} not in runfiles"));
    assert!(p.exists(), "{rel} resolved to a missing path {p:?}");
    p
}

fn open(device: Device) -> Embedder {
    Embedder::open_with(&model(), device, 2, Batching::ONE).unwrap()
}

fn open_sharing(prompts: usize) -> Embedder {
    let b = Batching {
        prompts,
        tokens: CONTEXT_TOKENS as usize,
    };
    let e = Embedder::open_with(&model(), Device::Cpu, 2, b).unwrap();
    assert_eq!(e.batching(), b);
    e
}

/// Texts of the shapes a chunk has, short and long.
fn texts() -> Vec<String> {
    vec![
        "lunch on friday".into(),
        "  two leading spaces,  and   runs\tof\twhitespace\n\nnew paragraph".into(),
        "naïve façade — résumé, Straße, Ångström".into(),
        "明日の会議は十時からです。よろしくお願いします。".into(),
        "launch 🚀 status: ✅ done, 🙂 thanks".into(),
        "**Worf:** _today_ is a [good day](https://example.com)".into(),
        "The away team beamed down to the surface at dawn. ".repeat(60),
        "warp core breach".into(),
    ]
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    dot / (norm(a) * norm(b))
}

fn norm(v: &[f32]) -> f64 {
    v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt()
}

// ── The model ───────────────────────────────────────────────────────

#[test]
fn a_vector_has_768_numbers_and_an_input_up_to_2048_tokens() {
    let e = open(Device::Cpu);
    assert_eq!(e.dim(), 768);
    assert_eq!(e.context(), 2048);
    assert_eq!(CONTEXT_TOKENS, 2048);
}

#[test]
fn every_input_starts_with_bos_and_ends_with_eos() {
    let e = open(Device::Cpu);
    let empty = e.tokenize("").unwrap();
    let [bos, eos] = empty[..] else {
        panic!("the empty text is {empty:?}, not BOS and EOS");
    };
    for text in texts() {
        let t = e.tokenize(&text).unwrap();
        assert_eq!((t[0], t[t.len() - 1]), (bos, eos), "{text:?}");
    }
}

#[test]
fn text_that_spells_a_special_token_is_plain_text() {
    let e = open(Device::Cpu);
    let empty = e.tokenize("").unwrap();
    let t = e
        .tokenize("the literal text <eos> and <bos> and <start_of_turn> is just text")
        .unwrap();
    let inner = &t[1..t.len() - 1];
    assert!(
        !inner.contains(&empty[0]) && !inner.contains(&empty[1]),
        "{t:?}"
    );
}

/// The chunker counts its budget with the tokenizer alone, without
/// loading the weights; it must count what the model counts, and each
/// token's end must be an offset into the text the chunker cuts at.
#[test]
fn the_tokenizer_alone_counts_as_the_model_does_and_its_pieces_spell_the_text() {
    let e = open(Device::Cpu);
    let t = Tokenizer::open(&model()).unwrap();
    let dense: String = (0..400).map(|i| format!("x{i:x}/")).collect();
    let mut all = texts();
    all.extend([
        String::new(),
        "the literal text <eos> and <bos> is just text".into(),
        // The mark SentencePiece reads a space as, written in the text.
        "Modifiers::default() ▁▁ indented with ▁ marks".into(),
        dense,
    ]);
    for text in all {
        let ends = t.ends(&text).unwrap();
        assert_eq!(ends.len() + 2, e.tokenize(&text).unwrap().len(), "{text:?}");
        assert_eq!(ends.last().copied().unwrap_or(0), text.len(), "{text:?}");
        assert!(ends.windows(2).all(|w| w[0] < w[1]), "{text:?}");
    }
}

/// The model's pooling leaves a vector about 1,100 long, and the length
/// varies by input; search compares by cosine, which reads only direction.
#[test]
fn a_vector_is_not_unit_length() {
    let mut e = open(Device::Cpu);
    for text in texts() {
        let n = norm(&e.embed(&text).unwrap());
        assert!(n > 100.0, "norm {n} for {text:?}");
    }
}

/// A chunk of dense text (code, an encoded blob) can hold more tokens than
/// the model's context though it is within the chunker's bytes; it failed
/// a whole source's embed. It is cut to fit, and its vector is the one of
/// the cut input.
#[test]
fn an_input_past_the_context_is_refused_and_fit_cuts_it_keeping_its_end() {
    let mut e = open(Device::Cpu);
    let dense: String = (0..1500).map(|i| format!("x{i:x}/")).collect();
    let tokens = e.tokenize(&dense).unwrap();
    assert!(tokens.len() > e.context(), "{} tokens", tokens.len());
    assert!(e.embed(&dense).is_err(), "the uncut input does not fit");
    let (vector, cut) = e.embed_fitted(&dense).unwrap();
    assert!(cut);
    let (fitted, _) = fit(tokens.clone(), e.context());
    assert_eq!(fitted.last(), tokens.last());
    assert_eq!(vector, e.embed_tokens(&fitted).unwrap());
}

/// The inference cache keys a vector by its prompt alone, so the same
/// prompt must give the same vector, to the bit, on the same device.
#[test]
fn the_same_input_in_the_same_call_gives_the_same_vector_to_the_bit() {
    for device in [Device::Cpu, Device::Best] {
        let mut e = open(device);
        for text in texts() {
            assert_eq!(
                e.embed(&text).unwrap(),
                e.embed(&text).unwrap(),
                "{device:?}: {text:?}"
            );
        }
    }
}

/// Tests run the embedder on fewer threads than a sync does
/// (`DATALIB_EMBED_THREADS`), so a vector must not depend on the count:
/// each row of a product is summed by one thread, whichever it is.
#[test]
fn the_thread_count_does_not_change_a_vector() {
    let at = |threads| {
        let mut e = Embedder::open_with(&model(), Device::Cpu, threads, Batching::ONE).unwrap();
        texts()
            .iter()
            .map(|t| e.embed(t).unwrap())
            .collect::<Vec<_>>()
    };
    let one = at(1);
    assert_eq!(one, at(4), "1 thread against 4");
    assert_eq!(one, at(3), "1 thread against 3");
}

// ── One call ────────────────────────────────────────────────────────

/// So a call attends over all of its tokens, and its time and scratch
/// memory grow with the square of them (`Batching`).
#[test]
fn llama_cpp_keeps_no_cache_for_the_model() {
    assert!(!open(Device::Cpu).keeps_cache());
    assert!(!open_sharing(4).keeps_cache());
}

/// Beside two different neighbours of the same length, a prompt comes out
/// the same to the bit, so no prompt attends to another.
#[test]
fn a_prompt_takes_nothing_from_what_shares_its_call() {
    let mut e = open_sharing(2);
    let prompt = "lunch on friday";
    let red = "red ".repeat(80);
    let blue = "blue ".repeat(80);
    assert_eq!(
        e.tokenize(&red).unwrap().len(),
        e.tokenize(&blue).unwrap().len(),
        "the neighbours must be the same length, or the call's shape differs"
    );
    let beside_red = e.embed_fitted_many(&[prompt, &red]).unwrap();
    let beside_blue = e.embed_fitted_many(&[prompt, &blue]).unwrap();
    assert!(cosine(&beside_red[1].0, &beside_blue[1].0) < 0.99);
    assert_eq!(beside_red[0].0, beside_blue[0].0);
}

/// The CPU picks its kernels by a call's token count, so a prompt in a call
/// of several rounds as it does not alone: 0.9995 at worst on arm64.
#[test]
fn a_prompt_sharing_a_call_gets_its_vector_alone_to_rounding() {
    let texts = texts();
    let mut alone = open(Device::Cpu);
    let mut together = open_sharing(4);
    let lens: Vec<usize> = texts
        .iter()
        .map(|t| together.tokenize(t).unwrap().len())
        .collect();
    let calls = pack(&lens, together.batching());
    assert!(
        calls.iter().filter(|run| run.len() > 1).count() >= 2,
        "too few calls held more than one prompt: {calls:?}"
    );
    let one = alone.embed_fitted_many(&texts).unwrap();
    let many = together.embed_fitted_many(&texts).unwrap();
    for ((text, (a, _)), (b, _)) in texts.iter().zip(&one).zip(&many) {
        let c = cosine(a, b);
        assert!(c > 0.999, "cosine {c} for {text:?}");
    }
}

// ── Devices ─────────────────────────────────────────────────────────

/// A mac's build has Metal, and the best device on one is its GPU.
#[cfg(target_os = "macos")]
#[test]
fn a_mac_runs_the_model_on_metal() {
    assert!(datalib_embedder::gpu_available());
    assert!(open(Device::Best).device().starts_with("MTL"));
    assert_eq!(open(Device::Cpu).device(), "CPU");
}

#[test]
fn the_best_device_agrees_with_the_cpu() {
    let mut gpu = open(Device::Best);
    let mut cpu = open(Device::Cpu);
    for text in texts() {
        let c = cosine(&gpu.embed(&text).unwrap(), &cpu.embed(&text).unwrap());
        assert!(c > 0.999, "cosine {c} for {text:?}");
    }
}

// ── Threads ─────────────────────────────────────────────────────────

/// ggml's threads sleep at its barrier (third-party/llama_cpp's patch), so
/// four of them pinned to one core embed about as fast as one. On
/// upstream's busy wait, four took 17 times as long as one here.
#[cfg(target_os = "linux")]
#[test]
fn four_threads_on_one_core_wait_asleep() {
    std::thread::spawn(|| {
        pin_this_thread_to_one_core();
        let time = |threads| {
            let mut e = Embedder::open_with(&model(), Device::Cpu, threads, Batching::ONE).unwrap();
            let start = std::time::Instant::now();
            for text in texts() {
                e.embed(&text).unwrap();
            }
            start.elapsed()
        };
        let one = time(1);
        let four = time(4);
        assert!(
            four < one * 3,
            "on one core, four threads took {four:?} and one took {one:?}"
        );
    })
    .join()
    .unwrap();
}

/// The threads ggml starts from this one inherit its CPU mask.
#[cfg(target_os = "linux")]
fn pin_this_thread_to_one_core() {
    let size = std::mem::size_of::<libc::cpu_set_t>();
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(libc::sched_getaffinity(0, size, &mut set), 0);
        let first = (0..libc::CPU_SETSIZE as usize)
            .find(|&cpu| libc::CPU_ISSET(cpu, &set))
            .expect("a CPU this thread may run on");
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(first, &mut set);
        assert_eq!(libc::sched_setaffinity(0, size, &set), 0);
    }
}

// ── Exit ────────────────────────────────────────────────────────────

const HOLD_A_MODEL_TO_EXIT: &str = "EMBEDDING_FACTS_HOLD_A_MODEL_TO_EXIT";

/// On Metal, ggml's teardown asserted that no model was still loaded and
/// aborted the process, so a test binary keeping its model in a static
/// (the applet's) failed after every test had passed. This test runs
/// itself again as a child that exits holding a model it has used.
#[test]
fn a_process_that_exits_with_a_model_loaded_exits_cleanly() {
    if std::env::var_os(HOLD_A_MODEL_TO_EXIT).is_some() {
        let mut e = open(Device::Best);
        e.embed("warp core breach").unwrap();
        std::mem::forget(e);
        std::process::exit(0);
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_process_that_exits_with_a_model_loaded_exits_cleanly",
            "--nocapture",
        ])
        .env(HOLD_A_MODEL_TO_EXIT, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the child {}:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
