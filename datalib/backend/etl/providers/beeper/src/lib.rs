//! Beeper provider for [`datalib_etl`]: Download (raw Matrix API
//! capture from `matrix.beeper.com`) and Render (raw → markdown +
//! grid_rows sidecars, dispatched per bridge network).

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
