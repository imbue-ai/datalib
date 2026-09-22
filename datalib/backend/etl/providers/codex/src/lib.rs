//! Codex sessions: the ingest half. Reads the rollouts Codex keeps
//! under `~/.codex/sessions` into a raw store, one row per line.
//! Rendering lives in [`datalib_etl_codex_render`].

pub mod ingest;
pub mod processor;
