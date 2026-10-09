//! Every hermetic YoLink test of the download, one binary: each module
//! is one behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen by a
//! process-global environment variable that each test points at its
//! own tape; one process means one such variable.

mod interrupt;
mod upgrade;
