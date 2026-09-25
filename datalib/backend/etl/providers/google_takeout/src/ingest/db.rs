//! This provider's raw store: the shared entity store and blob CAS,
//! with no queries of its own.

use super::schema_raw::full_ddl;

pub use datalib_etl::doltlite_raw::db_path_for;

/// Every cursor scope this provider owns. Reset wipes them in one go.
pub const CURSOR_SCOPE_PREFIX: &str = "google_takeout/";

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());
