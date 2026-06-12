//! JMAP translate: read raw doltlite db → render one markdown document
//! per JMAP Thread, plus a `grid_rows.json` sidecar, plus the thread's
//! attachment blobs materialized at `<thread>/blobs/<safe_filename>`.

pub mod parse;
pub mod render;

/// Bump to force a rebake of every rendered thread even when upstream
/// payloads are unchanged. The Load step keys `(qmd_path,
/// source_fingerprint)` on this version stamp, so a bump is the
/// canonical way to roll out a renderer change.
pub const RENDER_VERSION: u32 = 2;
