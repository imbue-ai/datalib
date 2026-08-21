//! `lightroom` — incremental, versioned backup of an Adobe Lightroom
//! Classic catalog.
//!
//! A `.lrcat` is an ordinary SQLite database. This provider mirrors it,
//! table for table, into a doltlite store, and lets doltlite's
//! content-addressed prolly trees do the deduplication: run it again
//! after a day of editing and only the rows that actually changed cost
//! anything, while every prior state stays queryable in `dolt_log` /
//! `dolt_history_<table>` / `dolt_diff_<table>`.
//!
//! Download-only for now — see [`processor`] for why render is deferred
//! and `INGEST.md` for what it will need.
//!
//! ## The engine is not Lightroom-specific
//!
//! [`download::mirror`] and [`download::plan`] know only "a SQLite file"
//! — every Lightroom-shaped decision arrives as config
//! (`datalib_etl_lightroom_config`). Any application whose data format
//! is a SQLite database (Quicken for Mac, Apple Photos, Things, …) is a
//! new config stanza, not new code. When the second such source lands,
//! lift `mirror.rs` + `plan.rs` into `datalib_etl` and let both provider
//! crates depend on it; keeping it here until then avoids inventing a
//! shared abstraction from a single example.
//!
//! ## What "generic" costs
//!
//! Three things are deliberately not mirrored:
//!
//! - **Indexes.** doltlite keys each table by its primary key in a
//!   prolly tree; a secondary index would cost space in every commit and
//!   buy a backup nothing.
//! - **Triggers and views.** They are behavior, and the mirror is never
//!   written to by an application.
//! - **CHECK / FOREIGN KEY constraints and collations.** Introspection
//!   (`PRAGMA table_info`) doesn't surface them, and enforcing the
//!   source's integrity rules on a copy of already-valid data buys
//!   nothing. See [`download::plan`] for why introspection beats
//!   replaying the source DDL verbatim.

pub mod download;
pub mod processor;
