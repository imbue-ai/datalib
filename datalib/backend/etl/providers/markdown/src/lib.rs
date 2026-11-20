//! `markdown` — the download half: scan a folder of markdown files (an
//! Obsidian vault, a folder of notes) and store each file's text under
//! its path. Turning a note into a page lives in
//! [`datalib_etl_markdown_render`].

pub mod ingest;
pub mod processor;
