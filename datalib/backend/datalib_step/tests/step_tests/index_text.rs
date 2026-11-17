//! What search sees of the fixture, one snapshot per source: every chunk
//! of every document, as the prompt the model embeds. A change to a
//! renderer, to the drawing (`search_index/src/text.rs`) or to where
//! chunks fall (`chunk.rs`) shows up here in review, rather than as a
//! search that quietly finds less. Drawn by `datalib-step index-text
//! --chunks`, the command a person runs, over the fixture's rendered root.

use std::collections::BTreeMap;
use std::process::Command;

#[test]
fn every_sources_index_text_is_this() {
    let td = tempfile::tempdir().unwrap();
    let tarball = std::env::var_os("INGESTED_QMD_TAR").expect("INGESTED_QMD_TAR");
    tar::Archive::new(std::fs::File::open(tarball).unwrap())
        .unpack(td.path())
        .unwrap();
    // Where the steps keep the model for this root: `--chunks` cuts the
    // chunks with its tokenizer.
    let model =
        std::fs::canonicalize(std::env::var_os("EMBED_MODEL").expect("EMBED_MODEL")).unwrap();
    let models = td.path().join("qmd/unified_grid_and_search_index/models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(
        model,
        models.join("hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"),
    )
    .unwrap();
    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("index-text")
        .arg(td.path().join("qmd"))
        .arg("--chunks")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();

    let mut by_source: BTreeMap<String, String> = BTreeMap::new();
    let mut source = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(head) = line.strip_prefix("=== ") {
            source = head.split(' ').next().unwrap_or_default().to_string();
        }
        by_source
            .entry(source.clone())
            .or_default()
            .push_str(&without_sizes(&without_hash(line)));
    }
    assert!(
        by_source.len() >= 20,
        "the fixture's sources: {:?}",
        by_source.keys().collect::<Vec<_>>()
    );
    for (source, text) in by_source {
        insta::assert_snapshot!(format!("index_text__{source}"), text);
    }
}

/// `--- chunk 3 <hash>` as `--- chunk 3`: the hash is of the prompt below
/// it, sizes and all, so it moves with a size the snapshot hides.
fn without_hash(line: &str) -> String {
    match line.strip_prefix("--- chunk ") {
        Some(rest) => format!(
            "--- chunk {}\n",
            rest.split(' ').next().unwrap_or_default().trim_end()
        ),
        None => line.to_string(),
    }
}

/// A size on disk (`157.1 KiB`), which a storage report shows and which
/// moves with how a store's pages were laid out, as `<size>`.
fn without_sizes(line: &str) -> String {
    let words: Vec<&str> = line.split(' ').collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let unit = words.get(i + 1).copied().unwrap_or_default();
        if words[i].parse::<f64>().is_ok() && ["B", "KiB", "MiB", "GiB"].contains(&unit) {
            out.push("<size>");
            i += 2;
        } else {
            out.push(words[i]);
            i += 1;
        }
    }
    out.join(" ")
}
