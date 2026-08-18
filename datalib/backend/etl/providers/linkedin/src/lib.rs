//! LinkedIn data-export ("takeout") provider.
//!
//! A LinkedIn export is a directory of CSV files (Connections,
//! Messages, Skills, Positions, …). [`download`] ingests *every* CSV
//! generically — one `(id, payload)` raw table per file — with no
//! per-file code; see its module docs for the identity / quirk-handling
//! story. Three render paths render selected feeds: [`render`] turns
//! the message-shaped tables into markdown via the shared chat renderer,
//! [`posts`] groups your shares + the comments you left into one
//! chat-style thread per post, and [`connections`] turns the
//! `connections` table into first-class contacts via the shared contact
//! renderer.
//!
//! Wired into the pipeline as the `linkedin` source type:
//!
//! ```toml
//! [[steps]]
//! id = "linkedin.download"
//! command = "datalib-step download linkedin"
//! outputs = ["linkedin/raw"]
//! [steps.params.common]
//! input_path = "~/backups/Basic_LinkedInDataExport_06-16-2026"
//! ```

pub mod connections;
pub mod download;
pub mod posts;
pub mod processor;
pub mod render;
pub mod synthesize;
