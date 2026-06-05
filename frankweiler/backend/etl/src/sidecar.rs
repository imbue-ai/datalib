//! Cross-provider Translate → Load contract.
//!
//! Every Translate step emits one `.grid_rows.json` sidecar per
//! rendered markdown. The shape is fixed: a small header (markdown
//! uuid + fingerprint + render schema stamp) followed by the rows
//! themselves. The Load step doesn't need to know which provider
//! produced the sidecar — it only needs to read `Sidecar` and upsert
//! `rows`.
//!
//! ```jsonc
//! {
//!   "header": {
//!     "markdown_uuid": "…",            // primary key for the rendered .md
//!     "source_fingerprint": "…",       // hash of upstream payload
//!     "render_version": 1              // renderer-side schema stamp
//!   },
//!   "rows": [GridRow, …],
//!   "edges": [EdgeRow, …]              // optional; outgoing edges originating
//!                                      // from this markdown. Older sidecars
//!                                      // omit the field entirely.
//! }
//! ```

use frankweiler_schema::edges::EdgeRow;
use frankweiler_schema::grid_rows::GridRow;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SidecarHeader {
    /// Stable id for the rendered `.md` this sidecar describes. Slack
    /// Translate sets this to the thread uuid; per-period providers
    /// (beeper) set it to a `(room, period)` UUIDv5; whole-conversation
    /// providers (notion page, github PR, …) reuse their native uuid.
    pub markdown_uuid: String,
    pub source_fingerprint: String,
    pub render_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub header: SidecarHeader,
    pub rows: Vec<GridRow>,
    /// Outgoing edges originating from this markdown
    /// (`src_markdown_uuid == header.markdown_uuid`). Older sidecars
    /// pre-dating the edges table omit the field; serde default yields
    /// an empty Vec so the Load step skips edge writes. We also skip
    /// serializing when empty so renderers that don't emit edges
    /// produce byte-identical sidecars to the pre-edges era — important
    /// for golden tests and for incremental cache stability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeRow>,
}
