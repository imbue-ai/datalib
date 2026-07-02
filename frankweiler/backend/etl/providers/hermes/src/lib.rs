//! `hermes` provider for [`frankweiler_etl`]: imports local-agent conversation
//! transcripts exported from Hermes Agent (and OpenClaw-compatible runtimes)
//! into datalib's queryable store.
//!
//! Translate-only. Two import modes, both normalizing into the shared
//! [`frankweiler_etl_chat_common`] chat model and delegating Markdown /
//! `grid_rows` / sidecar plumbing to that crate:
//!
//! * **Managed local import (`sync: {}`, primary UX)** — discover the local
//!   Hermes/OpenClaw agent history on this machine (`$HOME/.hermes`,
//!   per-profile dirs, OpenClaw-compatible roots) and read each root's
//!   `state.db` **read-only** (the source DB file is never copied and never
//!   mutated) plus any legacy `sessions/*.json`. See [`local`].
//! * **Export directory (`common.input_path`, advanced fallback)** — read a
//!   directory of `.jsonl` / `.json` session files the user exported first.
//!
//! There is still no Extract step and no writes to any source DB. Privacy note:
//! "never copied/mutated" is about the source *files*. The transcript contents
//! are read and mirrored into datalib's `data_root` as Markdown + index rows,
//! so the rendered output is as sensitive as the source history (system
//! prompts, memory, tool output) and should be protected accordingly.
//!
//! Hermes conversations are richer than ordinary chat — they carry tool calls,
//! reasoning, model/provider metadata, and the platform surface (`cli`,
//! `telegram`, `discord`, cron, …). Those survive into the grid via per-message
//! `kind_label`s ("User Input" / "LLM Response" / "LLM Thinking" / "Tool Call"
//! / "Tool Result" / "System") and the chat-level `project` (surface) /
//! `account` (user) / `external_id` (parent session id) columns.

pub mod local;
pub mod processor;
pub mod render_and_index_md;
