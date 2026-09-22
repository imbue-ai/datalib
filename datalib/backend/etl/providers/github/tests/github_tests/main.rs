//! Every hermetic GitHub test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod child_prune;
mod incremental_render;
mod live;
mod playback_roundtrip;
