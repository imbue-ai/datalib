//! Every hermetic email (JMAP, Gmail, mbox) test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod gmail_failed_fetch_is_owed;
mod gmail_history_replay;
mod gmail_interrupt;
mod gmail_label_lifecycle;
mod gmail_label_union;
mod gmail_run_problems;
mod gmail_widened_labels_backfill;
mod jmap_full_resync_prunes;
mod jmap_interrupt;
mod jmap_listed_then_fetched;
mod jmap_mbox;
mod jmap_progress_countdown;
mod jmap_render;
mod jmap_run_problems;
mod jmap_tape;
mod live;
mod playback_roundtrip;
mod progress_countdown;
mod support;
