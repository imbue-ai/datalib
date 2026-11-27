//! This provider's raw store: the shared entity store and blob CAS,
//! with no queries of its own.

use super::schema_raw::{full_ddl, LADDER};

pub use datalib_etl::doltlite_raw::db_path_for;

/// The prefix of every `ingested_files` scope this provider writes
/// (`google_takeout/<feed>`). Nothing reads it: `--reset` deletes every
/// row of every table in the raw store, `ingested_files` among them.
pub const CURSOR_SCOPE_PREFIX: &str = "google_takeout/";

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);
