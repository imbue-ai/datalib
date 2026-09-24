//! The supervisor: one loop that starts a step whenever an open request
//! wants it, it is stale, and nothing holds it back. The decision is
//! [`tick::tick`], a pure function; the hosts that feed it events and
//! act on its answer live beside it. Design: `docs/dev/plans/supervisor.md`.

pub mod bell;
pub mod host;
pub mod record;
pub mod reload;
pub mod round;
pub mod store;
pub mod tick;
