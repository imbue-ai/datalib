//! Every hermetic Slack test, one binary: each module is one
//! behaviour. `RUST_TEST_THREADS=1` because the playback transport is
//! chosen by a process-global environment variable that each test
//! points at its own fixture tree — three of these modules carried a
//! file-local mutex for exactly that, which stops working once they
//! share a process.

mod config_change_backfill;
mod dm_ingest;
mod history_prune;
mod interrupt;
mod playback_roundtrip;
mod probe;
mod slack_render;
mod slack_translate;
