//! CardDAV provider for [`datalib_etl`]: the download half — address
//! books from any RFC 6352-compliant server (Fastmail, iCloud, …) or a
//! folder of `.vcf` files into a doltlite raw store of vCard payloads.
//! Rendering lives in [`datalib_etl_contacts_render`].

pub mod ingest;
pub mod probe;
pub mod processor;
