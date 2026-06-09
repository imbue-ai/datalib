//! Cross-check the Rust sentence aligner against the Python reference
//! produced by `tools/perseus_alignment/build_reference.py` (in this
//! repo only as exploration output under ~/tmp/perseus/align/).
//!
//! Marked `#[ignore]` because:
//!   * It pulls ~440 MB of Ancient-Greek-BERT weights from HF Hub on
//!     first run.
//!   * It needs the reference fixture, expected at `PERSEUS_REFERENCE`
//!     (defaults to `~/tmp/perseus/align/sentence_alignments_reference.json`).
//!
//! Run locally with:
//!
//!     cargo test --release -p frankweiler-etl-perseus \
//!         --test cross_check_python -- --ignored --nocapture
//!
//! The success bar is **≥95% exact-match** of the grouping sequence
//! per section. Bit-equivalence between candle and HF transformers
//! has been verified on a single sentence (see the embed module
//! doc), so most drift expected is from edge-case ties in the DP,
//! not from numerical noise.

use std::path::PathBuf;

use serde::Deserialize;

use frankweiler_etl_perseus::translate::align::{align_section, Embedder};
use frankweiler_etl_perseus::translate::parse::Section;

#[derive(Deserialize)]
struct Reference {
    model: String,
    split_grc_punct: String,
    split_eng_punct: String,
    alignments: Vec<RefSection>,
}

#[derive(Deserialize)]
struct RefSection {
    #[serde(rename = "ref")]
    _ref: String,
    grc_sentences: Vec<String>,
    eng_sentences: Vec<String>,
    /// `[[grc_indices, eng_indices], ...]`
    pairs: Vec<(Vec<usize>, Vec<usize>)>,
}

fn fixture_path() -> PathBuf {
    if let Ok(p) = std::env::var("PERSEUS_REFERENCE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME unset");
    PathBuf::from(home).join("tmp/perseus/align/sentence_alignments_reference.json")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads model + needs reference fixture; run with --ignored"]
async fn matches_python_reference_on_thucydides() {
    let path = fixture_path();
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "couldn't read reference fixture at {} (set PERSEUS_REFERENCE if it lives elsewhere): {e}",
            path.display()
        )
    });
    let reference: Reference = serde_json::from_str(&body).expect("parse fixture");
    assert_eq!(
        reference.model, "pranaydeeps/Ancient-Greek-BERT",
        "fixture was built with a different model than the Rust embedder targets",
    );
    assert_eq!(reference.split_grc_punct, ".·;:");
    assert_eq!(reference.split_eng_punct, ".?!");
    eprintln!(
        "fixture: {} non-trivial sections",
        reference.alignments.len()
    );

    let emb = Embedder::load().await.expect("load embedder");

    let mut matched = 0usize;
    let mut mismatched: Vec<(String, Vec<(Vec<usize>, Vec<usize>)>, Vec<(Vec<usize>, Vec<usize>)>)> =
        Vec::new();

    for rec in &reference.alignments {
        // Round-trip via the actual Section type so we exercise the
        // same splitter the production path uses, on the joined raw
        // text (the Python reference saved sentences post-split).
        let grc = rec.grc_sentences.join(" ");
        let eng = rec.eng_sentences.join(" ");
        let section = Section {
            n: "x".into(),
            grc,
            eng,
        };
        let got = align_section(&emb, &section).expect("align");
        let got_pairs: Vec<(Vec<usize>, Vec<usize>)> = got
            .groups
            .iter()
            .map(|g| (g.grc_indices.clone(), g.eng_indices.clone()))
            .collect();

        if got_pairs == rec.pairs {
            matched += 1;
        } else if mismatched.len() < 5 {
            mismatched.push((rec._ref.clone(), got_pairs, rec.pairs.clone()));
        }
    }

    let total = reference.alignments.len();
    let pct = (matched as f64 / total as f64) * 100.0;
    eprintln!("matched: {matched}/{total} ({pct:.2}%)");
    for (r, got, exp) in &mismatched {
        eprintln!("  mismatch {r}: got {got:?} expected {exp:?}");
    }
    assert!(
        pct >= 95.0,
        "cross-check below 95% — Rust aligner has drifted from Python reference"
    );
}
