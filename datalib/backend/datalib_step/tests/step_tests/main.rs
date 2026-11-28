//! Every hermetic datalib-step test, one binary: each module is one
//! behaviour.

mod calendar_split;
mod check_verb;
mod clear_before_ingest;
mod compact;
mod documents_from_the_store;
mod embed_prompts;
mod embedding_map;
mod final_message;
mod index_text;
mod interrupt;
mod item_table;
mod lost_listing;
mod markdown_command;
mod no_op_ingest;
mod offline;
mod probe;
mod reset;
mod runtime_fetch;
mod scan_progress;

/// The SMS fixture as real files under `dir`, as a person's export is: the
/// tree bazel stages is all links, and an export's reader follows none.
fn sms_fixture_in(dir: &std::path::Path) -> std::path::PathBuf {
    let staged = std::env::var_os("SMS_FIXTURE_DIR").expect("SMS_FIXTURE_DIR");
    let export = dir.join("export");
    std::fs::create_dir_all(&export).unwrap();
    for entry in std::fs::read_dir(staged).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), export.join(entry.file_name())).unwrap();
    }
    export
}
