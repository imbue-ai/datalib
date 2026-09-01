//! `media` — scan a directory tree for audio, images, video and
//! playlists, and record what each one is.
//!
//! ## Relationship to `fsindex` and `pdf`
//!
//! All three scan a local tree, and all three share the primitives that
//! make that fast and correct — blake3 leaf hashing and Unison's
//! `(mtime, size, inode, dev)` rescan cursor — via
//! [`datalib_etl::fswalk`], which was factored out of `fsindex` when
//! `pdf` needed a second copy. This provider is its third user and adds
//! nothing to it.
//!
//! They are separate sources because they answer different questions.
//! `fsindex` answers "what is in this tree?" at tens-of-millions-of-rows
//! scale and keys everything on path. `pdf` answers "what documents do
//! I have?" and carries a render side. This one answers "what music,
//! photos and video do I have?" — content-keyed like `pdf`, but at a
//! scale between the two (a photo library is hundreds of thousands of
//! files, not thousands), with per-format container parsing `pdf` has
//! no equivalent of, and no render side at all.
//!
//! ## What this build does not do
//!
//! **No render side.** Media has no text to convert, so nothing here
//! reaches `grid_rows` or the qmd index — this provider fills its raw
//! store and stops, the way `fsindex` does. The data is queried
//! directly (see `DOWNLOAD.md` §"Inspecting a scan"). What it wants
//! instead is a UI of its own, which is a separate piece of work.

pub mod download;
pub mod processor;
