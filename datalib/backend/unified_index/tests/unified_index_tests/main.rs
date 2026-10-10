//! Every hermetic unified-index test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because `qmd_daemon_scope` sets
//! `DATALIB_RUNTIME_DIR` for the whole process, which is sound only
//! while no other test's thread is reading the environment.

mod dolt_backend_integration;
mod fixture_db_snapshot;
mod qmd_daemon_scope;
mod qmd_index_state;
mod qmd_mapping;
mod qmd_vectors;
