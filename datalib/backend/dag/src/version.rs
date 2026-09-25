//! The one version the loop makes up rather than takes from a step.

/// The version for a tree whose producer did not run this pass and has no
/// version recorded from an earlier one: the loop does not know what the
/// tree holds, and never reads it to find out.
///
/// Compared for equality like any other version, so two runs that both
/// know nothing agree. A real version always contains a colon
/// (`<fingerprint>:<version>`), so it can never collide with this.
pub const UNKNOWN: &str = "unknown";
