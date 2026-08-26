//! The unified index: the grid index and the qmd index, the query
//! language over them, and the repo that reads them.
//!
//! One crate because one request needs both — a free-text search takes
//! qmd hits, resolves them through `grid_row_refs`, then fetches rows
//! via `search_by_uuids`. Splitting them would put a process hop in the
//! middle of every query.
//!
//! Linked by `datalib-step` (which writes the indexes) and
//! `datalib-applet` (which serves them). `datalib-http` and
//! `datalib-dag` do not depend on this crate.

pub mod db;
pub mod dolt_repo;
pub mod qmd;
pub mod query;
pub mod repo;
pub mod search;
