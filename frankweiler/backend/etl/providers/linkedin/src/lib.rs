//! LinkedIn data-export ("takeout") provider.
//!
//! A LinkedIn export is a directory of CSV files (Connections,
//! Messages, Skills, Positions, …). [`extract`] ingests *every* CSV
//! generically — one `(id, payload)` raw table per file — with no
//! per-file code; see its module docs for the identity / quirk-handling
//! story. [`render`] is the only translate path today: it turns the
//! `messages` table into markdown via the shared chat renderer.
//!
//! Wired into the config-driven `sync` orchestrator as the `linkedin`
//! source type:
//!
//! ```yaml
//! - name: linkedin
//!   type: linkedin
//!   input_path: ~/backups/Basic_LinkedInDataExport_06-16-2026
//! ```

pub mod extract;
pub mod render;
