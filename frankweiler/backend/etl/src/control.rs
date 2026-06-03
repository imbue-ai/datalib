//! Shared cross-provider knobs for `extract::fetch`.
//!
//! Every provider's `FetchOptions` embeds an [`ExtractControl`] under
//! the field name `control`. The sync binary populates it from CLI
//! flags; each provider's extract path branches on the field that
//! matters to it.
//!
//! Keep this struct *small*. It's the union of "knobs that don't
//! belong in any one provider's own options" — meaning every provider
//! either implements the behavior or explicitly chooses to ignore it.

/// Cross-provider extract-time knobs.
#[derive(Debug, Clone, Default)]
pub struct ExtractControl {
    /// When true, the provider's `extract::fetch` truncates every
    /// data + bookkeeping table in its raw doltlite DB before
    /// fetching, so the run re-downloads every row from upstream.
    /// Paired with a fresh `dolt_commit` at the end, the resulting
    /// `dolt diff` between the prior commit and the new one shows
    /// only upstream-content changes — because the bookkeeping
    /// sidecars are not part of the data diff.
    ///
    /// `sync_runs` / `endpoint_shapes` / `sync_scope_state` are NOT
    /// truncated — they're whole-table bookkeeping (audit log,
    /// API discovery metadata, and resume cursor), not per-row
    /// content, and preserving them across resets is useful for
    /// debugging.
    pub reset_and_redownload: bool,
}
