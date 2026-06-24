//! The render→Load callback type shared by the translate phase.
//!
//! Program A migrated every provider's render to its own translate
//! `DataProcessor` (provider-owned config + render path), so the old
//! opaque-stanza `renderer_for` registry that lived here — one
//! `RenderAndIndexMd` impl per provider, selected by `type:` string — is gone.
//! All that remains is the fused-Load callback every translate processor emits
//! finished documents through.

use anyhow::Result;

use frankweiler_etl::load::RenderedMarkdown;

/// One callback per rendered markdown — hands the document (its path + row set)
/// to the orchestrator's inline Load step.
///
/// `Send` so the same callback type flows into a translate `DataProcessor`'s
/// `RunCtx` (whose `run` future is `Send`). The orchestrator's Load closure is
/// already `Send` (it's moved into a `spawn_blocking` task), so this is a no-op
/// widening for every caller.
pub type OnDoc<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + Send + 'a;
