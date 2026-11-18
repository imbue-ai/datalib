//! `datalib-step markdown`, run as a person runs it over the fixture's
//! rendered root: what it writes for each document is the `.md` file render
//! wrote, byte for byte, so it can stand in for the files as the way the
//! documents leave the library. One document printed is the same text, and
//! one no store holds is an error.

use std::collections::BTreeMap;
use std::process::Command;

use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;

fn step() -> Command {
    Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
}

#[test]
fn the_markdown_command_writes_every_document_as_its_file() {
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
        for row in store.markdown_rows().unwrap() {
            let Some(file) = row.md_path.map(|p| root.join(p)).filter(|f| f.exists()) else {
                continue;
            };
            let exported = out.join(&source).join(format!("{}.md", row.markdown_uuid));
            let written = std::fs::read_to_string(&file).unwrap();
            assert_eq!(
                std::fs::read_to_string(&exported)
                    .unwrap_or_else(|e| panic!("{}: {e}", exported.display())),
                written,
                "{source}: {} differs from its file",
                row.markdown_uuid
            );
            *checked.entry(source.clone()).or_default() += 1;
            one.get_or_insert((source.clone(), row.markdown_uuid.clone(), written));
        }
        store.close();
    }
    assert!(
        checked.len() >= 20,
        "the fixture's rendering sources: {checked:?}"
    );

    let (source, document, written) = one.unwrap();
    let printed = step()
        .args(["markdown"])
        .arg(&root)
        .args(["--source", &source, "--document", &document])
        .output()
        .unwrap();
    assert!(printed.status.success());
    assert_eq!(String::from_utf8(printed.stdout).unwrap(), written);

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
