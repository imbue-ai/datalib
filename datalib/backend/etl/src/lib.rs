//! Datalib ETL framework crate. Per-provider download +
//! render code lives in sibling crates named `datalib-etl-<provider>`
//! (e.g. [`datalib_etl_slack`]). The framework provides:

pub mod blob_cas;
pub mod bulk;
pub mod control;
pub mod doltlite_raw;
pub mod download_metrics;
pub mod download_params;
pub mod download_run;
pub mod event_store;
pub mod event_tape;
pub mod events;
pub mod file_checkpoint;
pub mod fingerprint_cache;
pub mod fsscan;
pub mod fswalk;
pub mod grid_index;
pub mod http;
pub mod ids;
pub mod indexed_markdown;
pub mod indicatif_progress;
pub mod latchkey;
pub mod layout;
pub mod periodize;
pub mod pin;
pub mod processor;
pub mod progress;
pub mod raw_layout;
pub mod raw_store;
pub mod render_cursor;
pub mod retry;
pub mod scope_config;
pub mod scope_state;
pub mod section;
pub mod synthesize;
pub mod title;
