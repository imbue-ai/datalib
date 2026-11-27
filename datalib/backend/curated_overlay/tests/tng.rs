//! The TNG fixture's curated stores, checked in under
//! `tests/fixtures/curated/`, laid over the TNG root and checked to fit.

// A test with no progress bars to corrupt: it says on stderr which store it
// wrote back.
#![allow(clippy::disallowed_macros)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the checked-in stores are, under the workspace and the runfiles.
const CHECKED_IN: &str = "tests/fixtures/curated/datalib_curated";

fn runfile(rloc: &str) -> PathBuf {
    let r = runfiles::Runfiles::create().expect("runfiles");
    runfiles::rlocation!(r, rloc).unwrap_or_else(|| panic!("no runfile {rloc}"))
}

fn env_runfile(var: &str) -> PathBuf {
    runfile(&std::env::var(var).unwrap_or_else(|_| panic!("{var} is set by the BUILD")))
}

fn updating() -> bool {
    std::env::var("INSTA_UPDATE").is_ok_and(|v| v == "always")
}

/// Guards a checked-in contacts or mutes store against a build that
/// cannot open it, a link to a handle the corpus no longer has, a mute of
/// a problem it no longer finds, and a shape change nobody wrote back.
#[test]
fn the_tng_curated_stores_fit_the_tng_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("data");
    let made = Command::new(env_runfile("MATERIALIZE_RLOC"))
        .arg(&root)
        .output()
        .expect("run materialize_tng_root");
    assert!(
        made.status.success(),
        "materialize_tng_root failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    // A directory has no runfiles entry; one store in it does.
    let src = runfile(&format!(
        "_main/{CHECKED_IN}/datalib_contacts/contacts.doltlite_db"
    ))
    .parent()
    .and_then(Path::parent)
    .expect("the checked-in dir")
    .to_path_buf();
    datalib_curated_overlay::copy_in(&src, &root);
    datalib_curated_overlay::open_as_owners(&root);
    datalib_curated_overlay::assert_every_table_has_a_row(&root);
    let overlay = datalib_curated_overlay::overlay(&env_runfile("APPLET_RLOC"), &root);
    for (store, dump) in datalib_curated_overlay::dumps(&root) {
        let name = Path::new(&store)
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .expect("a curated store's app dir")
            .to_string();
        insta::with_settings!({ description => store.as_str() }, {
            insta::assert_json_snapshot!(name, dump);
        });
    }
    insta::assert_json_snapshot!("overlay", overlay);

    let changed = datalib_curated_overlay::changed_by_this_build(&root, &src);
    if updating() {
        let workspace = PathBuf::from(std::env::var("INSTA_WORKSPACE_ROOT").expect("set"));
        for (ours, checked_in) in &changed {
            let rel = checked_in
                .strip_prefix(&src)
                .expect("under the checked-in dir");
            let dst = workspace.join(CHECKED_IN).join(rel);
            std::fs::copy(ours, &dst).expect("write the store back");
            eprintln!("wrote {} back: this build changed it", dst.display());
        }
        return;
    }
    let names: Vec<String> = changed
        .iter()
        .map(|(_, c)| c.display().to_string())
        .collect();
    assert!(
        names.is_empty(),
        "this build changes the checked-in curated stores {names:?} as it opens \
         them. Read the store's snapshot diff, then write them back: \
         bazel run //datalib/backend/curated_overlay:tng_overlay_test.update"
    );
}
