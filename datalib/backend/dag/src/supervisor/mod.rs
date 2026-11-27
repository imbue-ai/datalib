//! The supervisor: one loop that starts a step whenever an open request
//! wants it, it is stale, and nothing keeps it waiting. The decision is
//! [`tick::tick`], a pure function; the hosts that feed it events and
//! act on the [`tick::Tick`] it returns live beside it. The rules: `dag/README.md`.

pub mod host;
pub mod inputs;
pub mod locks;
pub mod loop_host;
#[cfg(test)]
pub(crate) mod recorded;
pub mod reload;
pub mod round;
pub mod serve;
pub mod tick;
pub mod upgrade;
pub mod wipe;
