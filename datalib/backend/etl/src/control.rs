//! Shared cross-provider knobs for `download::fetch`.

/// Cross-provider download-time knobs.
#[derive(Debug, Clone, Default)]
pub struct DownloadControl {
    /// When true, the provider's `download::fetch` truncates every
    /// data + bookkeeping table in its raw doltlite DB before
    /// fetching, so the run re-downloads every entity row from
    /// upstream. Paired with a fresh `dolt_commit` at the end, the
    /// resulting `dolt diff` between the prior commit and the new
    /// one shows only upstream-content changes — because the
    /// bookkeeping sidecars are not part of the data diff.
    pub reset_and_redownload: bool,

    /// When true, the provider's `download::fetch` clears the
    /// `blake3` column on its CAS edge table before fetching, so
    /// every attachment is re-fetched on the wire even when its bytes
    /// are already in the sibling CAS file. The edge rows themselves
    /// stay — their `(owning, ref)` metadata is upstream-driven and
    /// unchanged — and the CAS is never truncated: re-fetched bytes
    /// hash to the same blake3 and `INSERT OR IGNORE` is a no-op, so
    /// this costs network IO but not disk.
    pub refetch_blobs: bool,

    /// How often a download seals what it has written so far, so a consumer
    /// can start on it before the whole fetch finishes.
    ///
    /// `None` means the default cadence. The dial is the user's — how much
    /// latency to trade for how much `dolt_log` is their call — but whether
    /// checkpointing happens *at all* is not, and
    /// [`reset_and_redownload`](Self::reset_and_redownload) overrides it: see
    /// `RunCtx::checkpoint_policy`.
    pub checkpoint_cadence: Option<crate::checkpointer::Cadence>,
}
