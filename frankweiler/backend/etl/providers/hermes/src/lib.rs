//! `hermes` provider for [`frankweiler_etl`]: imports local-agent conversation
//! transcripts exported from Hermes Agent (and OpenClaw-compatible runtimes)
//! into datalib's queryable store.
//!
//! Translate-only and file-backed: it reads an export directory of `.jsonl` /
//! `.json` session files at `common.input_path`, normalizes each session into
//! the shared [`frankweiler_etl_chat_common`] chat model, and delegates
//! Markdown / `grid_rows` / sidecar plumbing to that crate. There is no Extract
//! step — datalib never opens a live `$HERMES_HOME/state.db`; the user exports
//! sessions to a directory first (privacy-safe, no locking/WAL/profile
//! hazards).
//!
//! Hermes conversations are richer than ordinary chat — they carry tool calls,
//! reasoning, model/provider metadata, and the platform surface (`cli`,
//! `telegram`, `discord`, cron, …). Those survive into the grid via per-message
//! `kind_label`s ("User Input" / "LLM Response" / "LLM Thinking" / "Tool Call"
//! / "Tool Result" / "System") and the chat-level `project` (surface) /
//! `account` (user) / `external_id` (parent session id) columns.

pub mod processor;
pub mod render_and_index_md;
