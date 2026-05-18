//! Cross-provider Translate → Load contract.
//!
//! Every Translate step emits one `.grid_rows.json` sidecar per
//! document. The shape is fixed: a small header (document uuid +
//! fingerprint + render schema stamp) followed by the rows themselves.
//! The Load step doesn't need to know which provider produced the
//! sidecar — it only needs to read `Sidecar` and upsert `rows`.
//!
//! ```jsonc
//! {
//!   "header": {
//!     "document_uuid": "…",            // primary key for the document
//!     "source_fingerprint": "…",       // hash of upstream payload
//!     "render_version": 1              // renderer-side schema stamp
//!   },
//!   "rows": [GridRow, …]
//! }
//! ```

use frankweiler_schema::grid_rows::GridRow;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SidecarHeader {
    /// Stable id for the document this sidecar describes. The Slack
    /// Translate step sets this to the thread uuid; other providers
    /// (Notion page, GitHub issue, etc.) plug in their own
    /// document-level uuid.
    pub document_uuid: String,
    pub source_fingerprint: String,
    pub render_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub header: SidecarHeader,
    pub rows: Vec<GridRow>,
}
