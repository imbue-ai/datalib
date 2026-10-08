//! Datalib ETL framework crate. Per-provider download +
//! render code lives in sibling crates named `datalib-etl-<provider>`
//! (e.g. [`datalib_etl_slack`]). The framework provides:

// `#[derive(RawStoreHandle)]` names `::datalib_etl::…` so provider crates
// can use it. This makes that path resolve inside this crate too, which is
// what lets the derive's own test live next to the trait it implements.
extern crate self as datalib_etl;

pub mod blob_cas;
pub mod bulk;
pub mod checkpointer;
pub mod content_line;
pub mod control;
pub mod doltlite_raw;
pub mod download_metrics;
pub mod download_problems;
pub mod download_run;
pub mod entity_store;
pub mod event_store;
pub mod event_tape;
pub mod events;
pub mod ids;
pub mod indicatif_progress;
pub mod layout;
pub mod periodize;
pub mod pin;
pub mod processor;
pub mod progress;
pub mod prune;
pub mod raw_layout;
pub mod raw_store;
pub mod run_problems;
pub mod scope_state;
pub mod stop;
pub mod store_handle;
pub mod xml;
