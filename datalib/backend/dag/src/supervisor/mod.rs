//! The supervisor: one loop that starts a step whenever an open request
//! wants it, it is stale, and nothing holds it back. The decision is
//! [`tick::tick`], a pure function; the hosts that feed it events and
//! act on its answer live beside it. Design: `docs/dev/plans/supervisor.md`.

pub mod host;
pub mod record;
pub mod round;
pub mod store;
pub mod tick;

/// What the loop tells its host about the store's requests, for a host
/// that keeps a record of its own beside them (the server's jobs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestEvent {
    /// Taken on: every invocation from here on serves it.
    Admitted { id: String },
    /// Done with. A stopped request is reported once the steps only it
    /// wanted have exited, not when the stop was read: until then they
    /// are still checkpointing on its behalf.
    Closed {
        id: String,
        outcome: store::RequestOutcome,
        failed_step: Option<String>,
        /// Who asked a stopped request to stop.
        stopped_by: Option<String>,
    },
}
