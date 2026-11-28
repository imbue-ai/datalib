//! The part of `datalib-step` that names no provider: the step protocol
//! (`events`, `source`, `function`), the ingest and render drivers that run
//! a `PlannedSource`, and the steps that read every source alike (the index,
//! search, the embedding map). `datalib-step`'s own crate plans a source
//! (`dispatch`) and holds everything else that names a provider crate, so
//! a change to one provider recompiles that crate and not this one.
//!
//! An async function that `datalib-step` awaits returns a `BoxFuture`
//! rather than being an `async fn`. An `async fn`'s body is compiled in
//! the crate that awaits it, along with everything it awaits in turn, so
//! an unboxed driver would be compiled again with every provider change.
//! Boxing it here compiles it here, once.

pub mod compact;
pub mod config_backup;
pub mod embed_prompts;
pub mod embedding_map;
pub mod events;
pub mod exit_watchdog;
pub mod function;
pub mod hints;
pub mod index_steps;
pub mod index_text;
pub mod ingest;
pub mod introspect;
pub mod markdown;
pub mod modal;
pub mod planned_source;
pub mod published;
pub mod render;
pub mod render_diff;
#[cfg(test)]
mod render_model_test;
pub mod search_index;
pub mod source;
pub mod source_type;
pub mod step_entry;
pub mod topo_sort_config;
pub mod unified_grid_and_search_index;
