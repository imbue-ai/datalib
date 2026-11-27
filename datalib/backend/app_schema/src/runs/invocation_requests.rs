// Which requests each invocation was started for, one row per pair. A
// step started for a sync serves every open request whose scope wants it;
// one a job started (a reset, a compaction, the prepare pass) serves that
// job, whose id goes in the same column.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "invocation_requests",
    primary_key = "invocation_id, request_id"
)]
pub struct InvocationRequestRow {
    #[col(sql = "VARCHAR(64)")]
    pub invocation_id: String,
    /// A request's id, or a job's, as `system/supervisor.sqlite` names it.
    #[col(sql = "VARCHAR(64)")]
    pub request_id: String,
}
