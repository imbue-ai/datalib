//! Email provider for [`datalib_etl`]: the download half — JMAP, the
//! Gmail API, or an `.mbox` on disk, all writing one deduped raw store.
//! Rendering lives in [`datalib_etl_email_render`].

pub mod download;
pub mod mailbox_labels;
pub mod probe;
pub mod processor;

pub use download::db;
