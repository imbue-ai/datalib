//! Todoist provider for [`datalib_etl`]: the download half, which
//! mirrors an account from `api.todoist.com` into raw JSON tables. The
//! render half is `datalib_etl_todoist_render`; INGEST.md has the
//! whole account.

pub mod ingest;
pub mod probe;
pub mod processor;
