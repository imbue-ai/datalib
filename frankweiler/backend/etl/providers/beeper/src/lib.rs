//! Beeper provider for [`frankweiler_etl`]: Download (raw Matrix API
//! capture from `matrix.beeper.com`) and Render (raw → markdown +
//! grid_rows sidecars, dispatched per bridge network).
//!
//! Beeper is multiplexed: one Matrix access token unlocks N upstream
//! networks (iMessage, WhatsApp, Signal, Telegram, Discord, …) that
//! Beeper's server-side bridges relay into individual Matrix rooms.
//! Download is bridge-agnostic — it just walks the Matrix
//! Client-Server API. Render dispatches per-room by the
//! `bridge_network` we infer from each room's `m.bridge` state event.
//!
//! The Load step is provider-agnostic and lives at
//! [`frankweiler_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
