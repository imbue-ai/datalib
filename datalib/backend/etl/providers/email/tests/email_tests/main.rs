//! Every hermetic email (JMAP, Gmail, mbox) test, one binary: each module is one
//! behaviour.

mod gmail_failed_fetch_is_owed;
mod gmail_filter_and_budget;
mod gmail_history_replay;
mod gmail_interrupt;
mod gmail_label_lifecycle;
mod gmail_label_union;
mod gmail_run_problems;
mod gmail_since_window;
mod gmail_takeout_seed;
mod gmail_whole_threads;
mod gmail_widened_labels_backfill;
mod jmap_account_row;
mod jmap_interrupt;
mod jmap_listed_then_fetched;
mod jmap_mbox;
mod jmap_progress_countdown;
mod jmap_render;
mod jmap_run_problems;
mod jmap_start_over_prunes;
mod jmap_tape;
mod jmap_whole_threads;
mod live;
mod playback_roundtrip;
mod progress_countdown;
mod support;
