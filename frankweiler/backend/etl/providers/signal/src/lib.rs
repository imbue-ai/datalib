//! Signal provider for [`frankweiler_etl`]: Download only (for now).
//!
//! Reads Signal-Android's new directory-format backup snapshots from
//! `input_path`, decrypts them via [`frankweiler_signal_backup`], and
//! UPSERTs per-recipient / per-chat / per-chat-item rows into a
//! doltlite raw store. The AEP (Account Entropy Pool) is read from the
//! `SIGNAL_BACKUP_PASSPHRASE` env var at download time — never persisted.
//!
//! Render (frames → markdown + grid_rows) is a follow-up.

pub mod download;
pub mod processor;
pub mod render;
