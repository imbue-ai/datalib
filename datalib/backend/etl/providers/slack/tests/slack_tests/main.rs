//! Every hermetic Slack test, one binary: each module is one
//! behaviour.

mod account_state;
mod attachment_retry;
mod config_change_backfill;
mod dm_ingest;
mod every_cut;
mod fixture_pipeline;
mod history_prune;
mod history_walk;
mod idle_dms;
mod interrupt;
mod new_since_last_sync;
mod newest_first;
mod playback_roundtrip;
mod probe;
mod progress_countdown;
mod quiet_channels;
mod reply_search;
mod run_problems;
mod slack_render;
mod slack_translate;
mod support;
mod upgrade;
