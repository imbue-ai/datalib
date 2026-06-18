//! LinkedIn data-export ("takeout") provider.
//!
//! A LinkedIn export is a directory of CSV files (Connections,
//! Messages, Skills, Positions, …). [`extract`] ingests *every* CSV
//! generically — one `(id, payload)` raw table per file — with no
//! per-file code; see its module docs for the identity / quirk-handling
//! story. Two translate paths render selected feeds: [`render`] turns the
//! message-shaped tables into markdown via the shared chat renderer, and
//! [`connections`] turns the `connections` table into first-class
//! contacts via the shared contact renderer.
//!
//! Wired into the config-driven `sync` orchestrator as the `linkedin`
//! source type:
//!
//! ```yaml
//! - name: linkedin
//!   type: linkedin
//!   input_path: ~/backups/Basic_LinkedInDataExport_06-16-2026
//! ```

pub mod connections;
pub mod extract;
pub mod render;
pub mod synthesize;
