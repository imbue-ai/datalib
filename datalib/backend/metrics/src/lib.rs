//! The metric names a step reports and the server reads back by name.
//!
//! A metric whose only reader is a person looking at the Activity cell
//! needs no name here — that cell draws whatever series it is handed.
//! A name belongs here once a *column* is keyed on it, because then
//! the reporter and the reader have to spell it the same way and
//! nothing else makes them.

/// Documents the source's render store holds, whole store, as of the
/// moment it was last reported — not the count this run wrote. The
/// Manage screen's Documents column.
///
/// Reported by every render step, including the ones whose provider
/// renders nothing: a zero there is the true answer, and a missing
/// series means "never counted", which the column draws as blank.
pub const DOCUMENTS: &str = "documents";
