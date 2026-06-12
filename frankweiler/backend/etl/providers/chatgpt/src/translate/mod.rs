//! ChatGPT Translate: raw API capture → parsed rows → markdown +
//! grid_rows sidecars. Stages 3-4 of the porting plan fill in the
//! render + sidecar emit; `parse` is in place.

pub mod grid_rows;
pub mod parse;
pub mod render;
pub mod sentinels;
