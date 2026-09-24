//! The render half of the `calendar` provider: raw events into
//! [`datalib_etl_calendar_common`]'s model, and from there markdown and
//! grid rows.

pub mod processor;
pub mod render;
