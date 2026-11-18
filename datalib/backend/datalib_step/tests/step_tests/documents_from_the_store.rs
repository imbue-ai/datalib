//! Every document of the fixture, spelled back from what its render store
//! keeps (`IndexedMarkdownStore::document`), is the `.md` file its render
//! wrote, byte for byte. What lets the files stop being written: whatever
//! reads a document can read the store instead and get the same text.

use std::collections::BTreeMap;

use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;

#[test]
fn the_store_spells_every_document_as_its_file() {
    let td = tempfile::tempdir().unwrap();
    let tarball = std::env::var_os("INGESTED_RENDERED_TAR").expect("INGESTED_RENDERED_TAR");
    tar::Archive::new(std::fs::File::open(tarball).unwrap())
        .unpack(td.path())
        .unwrap();
    let root = td.path().join("root");
    let mut checked: BTreeMap<String, usize> = BTreeMap::new();
    let mut without_a_file: BTreeMap<String, usize> = BTreeMap::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let source = entry.unwrap().file_name().to_string_lossy().to_string();
        let rendered = root.join(&source).join("render_markdown");
        let Some(store) = IndexedMarkdownStore::open_for_reading(&rendered, None).unwrap() else {
            continue;
        };
        for row in store.markdown_rows().unwrap() {
            let Some(md_path) = row.md_path.as_deref() else {
                continue;
            };
            let file = root.join(md_path);
            if !file.exists() {
                *without_a_file.entry(row.provider.clone()).or_default() += 1;
                continue;
            }
            let written = std::fs::read_to_string(&file).unwrap();
            let pieces = store
                .document(&row.markdown_uuid)
                .unwrap()
                .unwrap_or_else(|| panic!("{source}: the store cannot spell {md_path}"));
            let spelled: String = pieces.iter().map(|p| p.md.as_str()).collect();
            assert!(
                spelled == written,
                "{source}: {md_path} spelled from the store differs from its file\n\
                 --- file\n{written}\n--- store\n{spelled}"
            );
            *checked.entry(source.clone()).or_default() += 1;
        }
        store.close();
    }
    assert!(
        checked.len() >= 20,
        "the fixture's rendering sources: {checked:?}"
    );
    // A photo's document is its row alone: Lightroom names a file it never
    // writes, and the viewer draws the picture instead.
    assert!(
        without_a_file.keys().all(|p| p == "lightroom"),
        "documents with no file: {without_a_file:?}"
    );
}
