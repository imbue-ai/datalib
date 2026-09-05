//! CardDAV provider for [`datalib_etl`]: downloads address books
//! from any RFC 4791 / RFC 6352-compliant server (iCloud, Fastmail,
//! Google CardDAV, …) into a doltlite raw store of vCard payloads.

pub mod download;
pub mod processor;
pub mod render;
