//! Linear provider for [`datalib_etl`]: a download-only mirror of a
//! workspace from `api.linear.app`. Nothing is rendered; the raw store
//! is the product.

pub mod ingest;
pub mod probe;
pub mod processor;
