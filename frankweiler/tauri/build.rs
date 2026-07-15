use std::fs;
use std::path::Path;

/// Everything `bundle.resources` expects under `binaries/`, staged by
/// the config's beforeBuildCommand at `tauri build` time.
const STAGED_BINARIES: &[&str] = &[
    "binaries/frankweiler-http",
    "binaries/frankweiler-sync",
    "binaries/latchkey-curl-shim",
];

fn main() {
    for staged in STAGED_BINARIES {
        let staged = Path::new(staged);

        // Tauri validates `bundle.resources` paths at compile time, but the
        // real binaries are staged into `binaries/` by the config's
        // beforeBuildCommand — which only runs under `tauri build`, not a
        // bare `cargo check`/`cargo build`. Drop a placeholder so those still
        // compile. A real `tauri build` stages the actual binary before this
        // runs (so we leave the content), and `bundled_sync_bin` finds
        // nothing to bundle in a non-`.app` dev run anyway.
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
                let _ = fs::remove_file(profile_dir.join(staged));
            }
        }
    }

    // `bundle.resources` also lists `runtime/` — the Node runtime +
    // latchkey/qmd package trees staged by stage-runtime.sh, which (like
    // the binaries above) only runs under `tauri build`. Tauri validates
    // the path at compile time, so make sure the directory exists for
    // bare `cargo check`/`cargo build`. No read-only dance needed: the
    // staged tree comes from curl/npm, not Bazel's read-only outputs.
    let _ = fs::create_dir_all("runtime");

    tauri_build::build()
}
