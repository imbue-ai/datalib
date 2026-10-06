//! The metric names a step reports and the server reads back by name.
//!
//! Names follow Prometheus's conventions, because `GET /metrics` serves
//! them as they are: a running total ends in `_total` (a counter), and
//! anything that can go down does not (a gauge); a unit is the suffix
//! before it (`_bytes`, `_seconds`). The export reads a series' type off
//! its name, so the suffix is the contract.
//!
//! A metric whose only reader is a person looking at the sync dashboard
//! needs no name here — it charts whatever series it is handed.
//! A name belongs here once a *column* is keyed on it, because then
//! the reporter and the reader have to spell it the same way and
//! nothing else makes them.

/// Documents the source's render store holds, whole store, as of the
/// moment it was last reported — not the count this run wrote. The
/// Manage screen's Documents column.
///
/// Reported by every render step, including the ones whose provider
/// renders nothing: a zero there is the true answer, and a missing
/// series means "never counted", which the column draws as blank.
pub const DOCUMENTS: &str = "documents";

/// What those documents count between them — messages, readings,
/// events; each document's `item_count`, summed. Reported beside
/// [`DOCUMENTS`], by the same steps, at the same moments. The Manage
/// screen's Items column, and its sparkline.
pub const ITEMS: &str = "items";

/// Work still ahead of a step: a gauge. A step reports its own; the
/// runner keeps one more per producer, `queued{from=<producer>}`, for
/// the seals the step has not read. The Manage screen's Queue column
/// sums them.
pub const QUEUED: &str = "queued";

/// What a step's own progress bar has counted done: a running total.
/// With the step's own [`QUEUED`], it is how fast that queue is worked
/// off.
pub const DONE: &str = "done_total";

/// Checkpoints a step has sealed this run: a running total the runner
/// keeps from the step's `checkpoint` events.
pub const CHECKPOINTS: &str = "checkpoints_total";

/// The rows the runner has taken off a `queued{from=<producer>}` queue
/// as the step read them: a running total, one per producer, with the
/// same label. A queue a pass empties all at once falls in a sawtooth,
/// which a sampled gauge can miss the bottom of; a total that only
/// grows loses nothing to sampling, and is what the ETA column reads
/// the pace off.
pub const DEQUEUED: &str = "dequeued_total";

/// Whether a series is a running total — a counter — by the naming rule
/// above. `GET /metrics` types a series by this, and the runner warns
/// when one goes down within a step's attempt.
pub fn is_counter(name: &str) -> bool {
    name.ends_with("_total")
}
