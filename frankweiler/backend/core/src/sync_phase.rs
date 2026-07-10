//! Canonical vocabulary for `frankweiler-sync`'s coarse pipeline phases.
//!
//! The sync binary announces every phase transition by printing
//! [`SyncPhase::marker`] as its own output line (via `status_line!`, so
//! the format is identical whether tracing is in JSON or pretty mode).
//! The http worker that spawns the binary recovers the phase with
//! [`SyncPhase::from_marker_line`] instead of guessing from arbitrary
//! log text — which used to misfire, e.g. an extract-time event named
//! `anthropic_users_synthesized` matched a "synth" keyword and jumped
//! the UI's bar to Ingest while the download was still running. Both
//! sides share this module so the wire format can't drift.

/// Prefix of a phase-marker output line. Everything after it (trimmed)
/// is a [`SyncPhase::name`].
pub const MARKER_PREFIX: &str = "[frankweiler-sync] phase: ";

/// Prefix of a phase-*failure* marker. The pipeline keeps running past
/// a failed phase (one source's extract error doesn't stop the others,
/// and qmd still indexes whatever rendered), so by exit time the plain
/// phase markers have advanced past the failure. This line pins the
/// blame to the phase where the error actually happened.
pub const FAILED_MARKER_PREFIX: &str = "[frankweiler-sync] phase-failed: ";

/// The four user-visible stages of a sync run, in pipeline order.
/// `Index` and `Embed` are announced from `frankweiler_qmd_indexer`
/// (they bracket qmd subcommands); the first two from the sync
/// orchestrator itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncPhase {
    Download,
    Ingest,
    Index,
    Embed,
}

impl SyncPhase {
    pub const ALL: [SyncPhase; 4] = [
        SyncPhase::Download,
        SyncPhase::Ingest,
        SyncPhase::Index,
        SyncPhase::Embed,
    ];

    /// Wire name used in marker lines.
    pub fn name(self) -> &'static str {
        match self {
            SyncPhase::Download => "download",
            SyncPhase::Ingest => "ingest",
            SyncPhase::Index => "index",
            SyncPhase::Embed => "embed",
        }
    }

    /// Display title for progress UIs.
    pub fn title(self) -> &'static str {
        match self {
            SyncPhase::Download => "Download",
            SyncPhase::Ingest => "Ingest",
            SyncPhase::Index => "Index",
            SyncPhase::Embed => "Embed",
        }
    }

    /// Position in [`Self::ALL`] (pipeline order, 0-based).
    pub fn index(self) -> usize {
        self as usize
    }

    /// The full marker line announcing this phase.
    pub fn marker(self) -> String {
        format!("{MARKER_PREFIX}{}", self.name())
    }

    /// The full marker line reporting this phase failed.
    pub fn marker_failed(self) -> String {
        format!("{FAILED_MARKER_PREFIX}{}", self.name())
    }

    /// Parse a phase out of one line of sync output. Tolerates leading
    /// text (log teeing may prepend timestamps) but requires the
    /// remainder after the prefix to be exactly a phase name.
    pub fn from_marker_line(line: &str) -> Option<SyncPhase> {
        Self::parse_after(line, MARKER_PREFIX)
    }

    /// Parse a phase-failure marker out of one line of sync output.
    pub fn from_failed_marker_line(line: &str) -> Option<SyncPhase> {
        Self::parse_after(line, FAILED_MARKER_PREFIX)
    }

    fn parse_after(line: &str, prefix: &str) -> Option<SyncPhase> {
        let at = line.find(prefix)?;
        let rest = line[at + prefix.len()..].trim();
        SyncPhase::ALL.into_iter().find(|p| p.name() == rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips() {
        for p in SyncPhase::ALL {
            assert_eq!(SyncPhase::from_marker_line(&p.marker()), Some(p));
            assert_eq!(
                SyncPhase::from_failed_marker_line(&p.marker_failed()),
                Some(p)
            );
            // The two marker kinds must not cross-parse.
            assert_eq!(SyncPhase::from_marker_line(&p.marker_failed()), None);
            assert_eq!(SyncPhase::from_failed_marker_line(&p.marker()), None);
        }
    }

    #[test]
    fn tolerates_leading_text_and_whitespace() {
        assert_eq!(
            SyncPhase::from_marker_line("12:00:01 [frankweiler-sync] phase: ingest "),
            Some(SyncPhase::Ingest)
        );
    }

    #[test]
    fn rejects_non_markers() {
        assert_eq!(SyncPhase::from_marker_line("phase: ingest"), None);
        assert_eq!(
            SyncPhase::from_marker_line("[frankweiler-sync] phase: warp"),
            None
        );
        // A phase name merely mentioned in ordinary log text must not match.
        assert_eq!(
            SyncPhase::from_marker_line("anthropic_users_synthesized"),
            None
        );
    }

    #[test]
    fn all_is_in_pipeline_order() {
        for (i, p) in SyncPhase::ALL.into_iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }
}
