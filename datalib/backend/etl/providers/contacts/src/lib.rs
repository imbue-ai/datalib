//! CardDAV provider for [`datalib_etl`]: the download half — address
//! books from any RFC 4791 / RFC 6352-compliant server (iCloud,
//! Fastmail, Google CardDAV, …) into a doltlite raw store of vCard
//! payloads. Rendering lives in [`datalib_etl_contacts_render`].

pub mod ingest;
pub mod processor;
