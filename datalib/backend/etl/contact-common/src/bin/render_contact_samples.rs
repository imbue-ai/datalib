//! Writes the [`samples`](datalib_etl_contact_common::samples) corpus as
//! rendered `.md` into the directory named by the first argument, for
//! `//datalib/ui:render_preview`.

use std::path::PathBuf;

use anyhow::{bail, Result};

// Printing the paths on stdout is this tool's whole output contract.
#[allow(clippy::disallowed_macros)]
fn main() -> Result<()> {
    let Some(out) = std::env::args().nth(1) else {
        bail!("usage: render_contact_samples <out-dir>");
    };
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    for path in datalib_etl_contact_common::samples::write_samples(&out)? {
        println!("{path}");
    }
    Ok(())
}
