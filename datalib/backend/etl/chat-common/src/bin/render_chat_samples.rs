//! Writes the [`samples`](datalib_etl_chat_common::samples) corpus as
//! rendered `.md` into the directory named by the first argument.
//! Driven by `//datalib/ui:chat_preview`, which turns the output into
//! HTML and PNGs.

use std::path::PathBuf;

use anyhow::{bail, Result};

// Printing the paths on stdout is this tool's whole output contract:
// the preview script reads them.
#[allow(clippy::disallowed_macros)]
fn main() -> Result<()> {
    let Some(out) = std::env::args().nth(1) else {
        bail!("usage: render_chat_samples <out-dir>");
    };
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    for path in datalib_etl_chat_common::samples::write_samples(&out)? {
        println!("{path}");
    }
    Ok(())
}
