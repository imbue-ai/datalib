//! Every hermetic Claude test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod claude_conv_uuid_problems;
mod claude_projects;
mod claude_render;
mod claude_translate;
mod playback_roundtrip;
mod reset_and_resync;
