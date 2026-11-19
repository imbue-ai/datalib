//! Every document of the fixture spells back from what its render store
//! keeps (`IndexedMarkdownStore::document_text`): with no `.md` files, the
//! store is the only place a document's markdown lives.

use std::collections::BTreeMap;

use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;

#[test]
fn the_store_spells_every_document() {
    let td = tempfile::tempdir().unwrap();
    let tarball = std::env::var_os("INGESTED_RENDERED_TAR").expect("INGESTED_RENDERED_TAR");
    tar::Archive::new(std::fs::File::open(tarball).unwrap())
        .unpack(td.path())
        .unwrap();
    let root = td.path().join("root");
    let mut spelled: BTreeMap<String, usize> = BTreeMap::new();
    let mut unspelled: BTreeMap<String, usize> = BTreeMap::new();
    for source in datalib_etl_render::grid_index::discover_sources(&root) {
        let rendered = root.join(&source).join("render_markdown");
        let store = IndexedMarkdownStore::open_for_reading(&rendered, None)
            .unwrap()
            .unwrap();
        for row in store.markdown_rows().unwrap() {
            match store.document_text(&row.markdown_uuid).unwrap() {
                Some(text) if !text.is_empty() => *spelled.entry(source.clone()).or_default() += 1,
                _ => *unspelled.entry(row.provider.clone()).or_default() += 1,
            }
        }
        store.close();
    }
    assert!(
        spelled.len() >= 20,
        "the fixture's rendering sources: {spelled:?}"
    );
    // A photo's document is its row alone: the viewer draws the picture.
    assert!(
        unspelled.keys().all(|p| p == "lightroom"),
        "documents the store cannot spell: {unspelled:?}"
    );
}
