//! Where datalib's files sit on disk, and how it spawns the Node-based
//! tools it ships with.
//!
//! A deliberately dependency-free crate: nothing in here reaches past
//! `std`, and nothing in here knows what a grid row, a source, or a
//! config is. Two reasons to keep it that way.
//!
//! The first is the usual one — the data-root layout is shared by every
//! writer and every reader, so it must not live in a crate that only
//! some of them can link.
//!
//! The second is the build cache, and it is why this crate was split out
//! of `datalib_core` rather than left in it. `qmd_indexer_bin` is a
//! `tools=` input to the fixture's embedding genrule
//! (`//tests/fixtures:ingested_tng_qmd`), and bazel keys an action on
//! the digests of its tools. So the set of crates that binary links is
//! exactly the set whose next edit re-runs a ~90s CPU-only embed on CI.
//! It reached for a handful of path joins, a `Command` builder and a
//! version constant, and paid for them by linking `datalib_core`
//! (160 rdeps) and `datalib_unified_index`. Now it links this instead.
//!
//! `datalib_core` re-exports [`layout`] and [`node_runtime`], and
//! `datalib_unified_index::qmd` re-exports [`qmd`], so every existing
//! call site still resolves and each constant still has exactly one
//! definition.

pub mod layout;
pub mod node_runtime;
pub mod qmd;
