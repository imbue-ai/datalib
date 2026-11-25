//! How the server hands a program it starts, an applet or a probe, the
//! params it was configured or asked with: in a file only its owner can
//! read, named on its command line. Never on argv itself: params carry
//! tokens, and argv is readable by every user on the machine. A step gets
//! its params from the runner in `DATALIB_DAG_PARAMS` instead.

use anyhow::Context as _;

/// The flag the params file is named on. The file is created `0600` and
/// removed when the handle [`write_params_file`] returns is dropped.
pub const PARAMS_FILE_FLAG: &str = "--params-file";

/// Where those files are made, under the data root.
pub const PARAMS_DIR_REL_PATH: &str = "system/params";

/// Write `json` to a fresh owner-only file under `<data_root>/system/params`.
/// The file lives as long as the returned handle.
pub fn write_params_file(
    data_root: &std::path::Path,
    who: &str,
    json: &str,
) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let dir = data_root.join(PARAMS_DIR_REL_PATH);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    // `tempfile` creates with `O_EXCL` and mode 0600, so the file is
    // never readable by anyone else, not even between create and write.
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{}.", who.replace('/', "_")))
        .suffix(".json")
        .tempfile_in(&dir)
        .with_context(|| format!("create a params file in {}", dir.display()))?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.flush())
        .with_context(|| format!("write {}", file.path().display()))?;
    Ok(file)
}
