//! Where Datalib's files sit on disk, how it spawns the Node-based tools
//! it ships with, where it finds the CUDA module, and how it replaces a
//! file in one rename.

pub mod atomic;
pub mod build_id;
pub mod cache;
pub mod cuda_module;
pub mod layout;
pub mod models;
pub mod node_runtime;
pub mod plain_sqlite;
pub mod runtime_manifest;
