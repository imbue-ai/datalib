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
    /// fetching, so the run re-downloads every entity row from
    /// upstream. Paired with a fresh `dolt_commit` at the end, the
    /// resulting `dolt diff` between the prior commit and the new
    /// one shows only upstream-content changes — because the
    /// bookkeeping sidecars are not part of the data diff.
    ///
    /// `sync_runs` / `sync_scope_state` are NOT truncated — they're
    /// whole-table bookkeeping (audit log and resume cursor), not
    /// per-row content, and preserving them across resets is useful
    /// for debugging.
    ///
    /// `blob_refs` is NOT truncated either: the per-source CAS
    /// retains the bytes across this reset, and `blob_refs.blake3`
    /// is the cache index that lets the next extract skip
    /// re-fetching. Use [`Self::refetch_blobs`] to invalidate the
    /// blob cache index when you actually want the bytes re-pulled.
    pub reset_and_redownload: bool,

    /// When true, the provider's `extract::fetch` wipes the
    /// `blob_refs` table (and its bookkeeping sidecar) before
    /// fetching, so every attachment is re-fetched on the wire even
    /// when its bytes are already in the sibling CAS file. The CAS
    /// itself is never truncated — re-fetched bytes hash to the same
    /// blake3 and `INSERT OR IGNORE` is a no-op, so this costs
    /// network IO but not disk.
    ///
    /// Orthogonal to [`Self::reset_and_redownload`]: pass both for a
    /// full reset; pass `reset_and_redownload` alone for the common
    /// "check for entity gaps without burning bandwidth on blobs"
    /// case.
    pub refetch_blobs: bool,
}
