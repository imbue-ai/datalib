//! Baby Connect provider for [`datalib_etl`]: the download half, which
//! copies every entry a family logged at app.babyconnect.com into raw
//! JSON tables. The render half is `datalib_etl_baby_connect_render`;
//! INGEST.md has the whole account.

pub mod ingest;
pub mod processor;
