use std::fs;
use std::path::Path;

fn main() {
    // Tauri validates `bundle.resources` paths at compile time, but the
    // real `frankweiler-sync` is staged into `binaries/` by the config's
    // beforeBuildCommand — which only runs under `tauri build`, not a
    // bare `cargo check`/`cargo build`. Drop a placeholder so those still
    // compile. A real `tauri build` stages the actual binary before this
    // runs (so we leave the content), and `bundled_sync_bin` finds
    // nothing to bundle in a non-`.app` dev run anyway.
    let staged = Path::new("binaries/frankweiler-sync");
    if !staged.exists() {
        let _ = fs::create_dir_all("binaries");
        let _ = fs::write(
            staged,
            b"placeholder: replaced by beforeBuildCommand at `tauri build`\n",
        );
    }

    // The staged binary is copied from Bazel's read-only output, so it —
    // and every copy Tauri makes from it — lands read-only. Tauri copies
    // resources next to the profile binary (`target/<profile>/binaries/`)
    // during this build script; on the next build it can't overwrite its
    // own read-only copy (EACCES). Force the staged source writable, and
    // remove any stale read-only profile-dir copy so Tauri regenerates it
    // writable. Both run before `tauri_build::build()` below.
    #[cfg(unix)]
    if staged.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(staged, fs::Permissions::from_mode(0o755));
    }
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out, so its 3rd
        // ancestor is target/<profile>, where Tauri drops the resource.
        if let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) {
            let _ = fs::remove_file(profile_dir.join("binaries/frankweiler-sync"));
        }
    }

    tauri_build::build()
}
