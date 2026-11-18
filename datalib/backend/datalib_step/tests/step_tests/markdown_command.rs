//! `datalib-step markdown`, run as a person runs it over the fixture's
//! rendered root: it writes each document as its render store spells it,
//! one file per document at `<out>/<source>/<id>.md`, so it is the way the
//! documents leave the library. One document printed is the same text, and
//! one no store holds is an error.

use std::collections::BTreeMap;
use std::process::Command;

use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;

fn step() -> Command {
    Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
}

#[test]
fn the_markdown_command_writes_every_document_its_store_spells() {
    let td = tempfile::tempdir().unwrap();
    let tarball = std::env::var_os("INGESTED_RENDERED_TAR").expect("INGESTED_RENDERED_TAR");
    tar::Archive::new(std::fs::File::open(tarball).unwrap())
        .unpack(td.path())
        .unwrap();
    let root = td.path().join("root");
    let out = td.path().join("exported");
    let ran = step()
        .arg("markdown")
        .arg(&root)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let mut checked: BTreeMap<String, usize> = BTreeMap::new();
    let mut one = None;
    for source in datalib_etl_render::grid_index::discover_sources(&root) {
        let rendered = root.join(&source).join("render_markdown");
        let store = IndexedMarkdownStore::open_for_reading(&rendered, None)
            .unwrap()
            .unwrap();
        for (uuid, text) in store.document_texts().unwrap() {
            let exported = out.join(&source).join(format!("{uuid}.md"));
            assert_eq!(
                std::fs::read_to_string(&exported)
                    .unwrap_or_else(|e| panic!("{}: {e}", exported.display())),
                text,
                "{source}: {uuid} is not exported as its store spells it"
            );
            *checked.entry(source.clone()).or_default() += 1;
            one.get_or_insert((source.clone(), uuid, text));
        }
        store.close();
    }
    assert!(
        checked.len() >= 20,
        "the fixture's rendering sources: {checked:?}"
    );

    let (source, document, text) = one.unwrap();
    let printed = step()
        .args(["markdown"])
        .arg(&root)
        .args(["--source", &source, "--document", &document])
        .output()
        .unwrap();
    assert!(printed.status.success());
    assert_eq!(String::from_utf8(printed.stdout).unwrap(), text);

    let missing = step()
        .arg("markdown")
        .arg(&root)
        .args(["--document", "00000000-0000-0000-0000-000000000000"])
        .output()
        .unwrap();
    assert!(
        !missing.status.success(),
        "a document no store holds is an error"
    );
}
