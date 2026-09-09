//! Beeper provider for [`datalib_etl`]: the download half — raw Matrix
//! API capture from `matrix.beeper.com`. Rendering lives in
//! [`datalib_etl_beeper_render`].

pub mod download;
pub mod processor;
pub mod synthesize;
