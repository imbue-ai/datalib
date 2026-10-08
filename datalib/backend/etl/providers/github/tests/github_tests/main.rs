//! Every hermetic GitHub test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod child_prune;
mod incremental_render;
mod interrupt;
mod live;
mod playback_roundtrip;
mod run_problems;
mod support;
mod sync_state;
mod upgrade;

/// The fixture's pinned clock, as the pipeline's `--now` sets it.
pub fn tng_now() -> datalib_time::IsoOffsetTimestamp {
    datalib_time::parse_strict("2369-04-15T00:00:00+00:00").unwrap()
}
