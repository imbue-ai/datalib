//! `fsindex` — directory-tree indexer.
//!
//! Scans a local directory tree and stores, for each visible entry,
//! `(path, kind, size, blake3, optional identity uuid)` in a doltlite
//! raw store. Built for tens of millions of rows, fast incremental
//! rescans (Unison's `(mtime, size, inode)` trick — see
//! [`extract::schema_raw`] for the cursor table that backs it), and
//! branch-level diff (`SELECT … FROM main.files m FULL JOIN b.files l
//! USING(id) WHERE m.blake3 IS NOT l.blake3`) as the comparison/sync
//! primitive.
//!
//! This crate is the **extract side only** for now; the schema
//! lives at [`extract::schema_raw`] and the rest of the walker /
//! stamp-compare / hasher / db / options modules will land in
//! follow-up commits. Translate is deliberately out of scope until
//! we know how filesystem entries project into the `GridRow` family
//! (they're not chat-shaped; closer to contacts-shaped — see
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! §"Entities without a time-shape").
//!
//! ## What this provider does that no other does
//!
//! - **Mutates the data root.** Opt-in via `stamp_me_with_uuid` in
//!   an ancestor `.fsindex.yaml`, the scanner writes UUID breadcrumb
//!   files into the tree it's scanning. Every other provider in the
//!   framework is read-only against its upstream. See
//!   [`EXTRACT.md`](../EXTRACT.md) §"Stamping policy."
//! - **No JSONL tape, no CAS sibling.** Per
//!   [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//!   §"Bulk-upsert as the standard write path," file-imported
//!   sources skip the wire-event chokepoint and write through
//!   `bulk_upsert_bookkeeping` directly. There is no upstream wire
//!   event to mirror and no separate attachment-fetch — every byte
//!   we'd hash is already on the user's disk.

pub mod extract;
