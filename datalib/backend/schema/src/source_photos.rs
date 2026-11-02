// Photos and videos as sources describe them (`NormalizedPhoto`, in
// `datalib_photo_schema`), carried by the document that is the photo, the
// way a document carries its contacts: a re-render replaces its own rows.
// In every render store and in the index; the photos applet reads it.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "source_photos", primary_key = "markdown_uuid, photo_uuid")]
pub struct SourcePhotoRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// `NormalizedPhoto::uuid`: the photo's grid row.
    #[col(sql = "VARCHAR(96)")]
    pub photo_uuid: String,
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    /// The whole `NormalizedPhoto`, as JSON, so the type can grow without
    /// a column per field.
    #[col(sql = "TEXT")]
    pub photo_json: String,
}
