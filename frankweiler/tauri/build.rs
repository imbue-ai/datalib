use std::fs;
use std::path::Path;

fn main() {
    // Tauri validates `bundle.resources` paths at compile time, but the
    // real `frankweiler-sync` is staged into `binaries/` by the config's
    // beforeBuildCommand — which only runs under `tauri build`, not a
    // bare `cargo check`/`cargo build`. Drop a placeholder so those still
    // compile. A real `tauri build` stages the actual binary before this
    // runs (so we leave it untouched), and `bundled_sync_bin` finds
    // nothing to bundle in a non-`.app` dev run anyway.
    let stub = Path::new("binaries/frankweiler-sync");
    if !stub.exists() {
        let _ = fs::create_dir_all("binaries");
        let _ = fs::write(
            stub,
            b"placeholder: replaced by beforeBuildCommand at `tauri build`\n",
        );
    }

    tauri_build::build()
}
