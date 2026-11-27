//! Email provider for [`datalib_etl`]: the download half — JMAP, the
//! Gmail API, an `.mbox` on disk or Apple Mail, all writing one deduped
//! raw store.
//! Rendering lives in [`datalib_etl_email_render`].

pub mod ingest;
pub mod mailbox_labels;
pub mod probe;
pub mod processor;

pub use ingest::db;
