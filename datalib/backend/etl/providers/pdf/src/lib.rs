//! `pdf` — the download half: scan a directory tree for PDFs and land
//! them in a content-keyed raw store. Converting the readable ones to
//! markdown lives in [`datalib_etl_pdf_render`].

pub mod ingest;
pub mod processor;
