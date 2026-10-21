//! Packages: programs from outside datalib that a data root runs as steps
//! and applets. A package is published as one manifest at a URL; the root's
//! config pins it by the manifest's sha256 (`[[packages]]`), and this crate
//! fetches, checks and unpacks it under `system/packages/`.
//! `docs/dev/packages.md` is the format and the rules.

mod install;
pub mod manifest;
pub mod platform;
pub mod source;

pub use install::{ensure, installed, lookup, Lookup};
pub use manifest::Manifest;
