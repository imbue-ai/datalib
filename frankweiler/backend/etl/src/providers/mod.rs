//! Per-provider Extract + Translate code. Each provider is a sibling
//! module here; the cross-provider Load step lives in [`crate::load`]
//! and consumes whatever sidecars these modules write.

pub mod slack;
