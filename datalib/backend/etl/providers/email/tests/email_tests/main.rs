//! Every hermetic email (JMAP, Gmail, mbox) test, one binary: each module is one
//! behaviour.
//!
//! `RUST_TEST_THREADS=1` because the playback transport is chosen
//! by a process-global environment variable that each test points
//! at its own fixture tree; one process means one such variable.

mod gmail_failed_fetch_holds_cursor;
mod gmail_label_union;
mod gmail_widened_labels_backfill;
mod jmap_mbox;
mod jmap_progress_countdown;
mod jmap_render;
mod live;
mod playback_roundtrip;
mod progress_countdown;
mod support;
