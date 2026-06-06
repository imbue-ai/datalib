//! Read the raw doltlite db into a `Parsed` bag for the renderer.

use std::path::Path;

use anyhow::Result;

use crate::extract::db::{block_on_load_all, LoadedRaw};

pub fn parse_export(db_path: &Path) -> Result<LoadedRaw> {
    block_on_load_all(db_path)
}
