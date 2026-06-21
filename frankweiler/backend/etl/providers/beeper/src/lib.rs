//! Beeper provider for [`frankweiler_etl`]: Extract (raw Matrix API
//! capture from `matrix.beeper.com`) and Translate (raw → markdown +
//! grid_rows sidecars, dispatched per bridge network).
//!
//! Beeper is multiplexed: one Matrix access token unlocks N upstream
//! networks (iMessage, WhatsApp, Signal, Telegram, Discord, …) that
//! Beeper's server-side bridges relay into individual Matrix rooms.
//! Extract is bridge-agnostic — it just walks the Matrix
//! Client-Server API. Translate dispatches per-room by the
//! `bridge_network` we infer from each room's `m.bridge` state event.
//!
//! The Load step is provider-agnostic and lives at
//! [`frankweiler_etl::load`].

pub mod extract;
pub mod render_and_index_md;
pub mod synthesize;
