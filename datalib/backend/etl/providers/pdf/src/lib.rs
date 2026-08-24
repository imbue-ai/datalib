//! `pdf` — scan a directory tree for PDFs, convert the readable ones to
//! markdown, and index the result.
//!
//! ## Relationship to `fsindex`
//!
//! Both providers scan a local tree, and they share the primitives that
//! make that fast and correct — blake3 leaf hashing and Unison's
//! `(mtime, size, inode, dev)` rescan cursor — via
//! [`datalib_etl::fswalk`], which was factored out of fsindex for this
//! purpose.
//!
//! They are separate sources because they answer different questions.
//! fsindex answers "what is in this tree?" at tens-of-millions-of-rows
//! scale, keys everything on path, tree-hashes directories, and has no
//! render side. `pdf` answers "what documents do I have?" at
//! thousands-of-rows scale, keys content on `blake3` so duplicates
//! collapse and moves are free, carries a render side, and needs real
//! per-document retry state because conversion can fail in ways a
//! `read(2)` cannot.
//!
//! ## What this build does not do
//!
//! **No OCR.** Scanned documents are classified, recorded with
//! `needs_ocr = 1`, and skipped. See `DOWNLOAD.md` §"Why no OCR yet"
//! for the measurements behind that choice and what adding an engine
//! would involve.

pub mod download;
pub mod processor;
pub mod render;
