//! Signal provider for [`datalib_etl`]: the download half — the
//! decrypted Android backup into a doltlite raw store. Rendering lives
//! in [`datalib_etl_signal_render`].

pub mod ingest;
pub mod processor;
