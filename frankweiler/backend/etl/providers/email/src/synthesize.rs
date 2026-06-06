//! Fixture → HTTP playback synthesis (stub).
//!
//! TODO: walk `tests/fixtures/` and emit `<key>.json` playback files
//! that `frankweiler_etl::http` will replay during integration tests.

#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;

/// Stub. Real implementation will mirror Slack's `synthesize.rs`:
/// load checked-in JSON fixtures and write out the playback envelopes.
pub fn synthesize(_fixture_dir: &Path, _out_dir: &Path) -> Result<()> {
    Ok(())
}
