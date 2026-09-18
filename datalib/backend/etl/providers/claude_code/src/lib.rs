//! Claude Code sessions: the ingest half. Reads the transcripts Claude
//! Code keeps under `~/.claude/projects` into a raw store, one row per
//! transcript record. Rendering lives in [`datalib_etl_claude_code_render`].

pub mod ingest;
pub mod processor;
