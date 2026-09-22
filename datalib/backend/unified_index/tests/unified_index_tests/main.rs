//! Every hermetic unified-index test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod dolt_backend_integration;
mod fixture_db_snapshot;
mod qmd_daemon_scope;
mod qmd_index_state;
mod qmd_mapping;
