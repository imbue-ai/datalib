//! Raw calendar objects → normalized events → markdown + grid rows.

pub mod google;
pub mod ics;
pub mod ids;
pub mod parse;
// The directory is the pipeline stage and the file the step within it,
// as in every provider.
#[allow(clippy::module_inception)]
pub mod render;
