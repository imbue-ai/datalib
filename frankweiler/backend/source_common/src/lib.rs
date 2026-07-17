//! Schema-only foundation crate shared by the orchestrator (`ingest_config`)
//! and every provider `*-config` crate.
//!
//! Holds the per-source **common envelope** of shared tunables ([`SourceCommon`])
//! that each provider config composes, the global [`Defaults`] block those
//! tunables fall back to, and the cross-source download knobs
//! ([`DownloadParams`]). Depends on nothing but `serde`, so any config crate can
//! compose [`SourceCommon`] without pulling ETL or orchestrator code.
//!
//! All cross-node derivation (folding [`Defaults`] into each source, resolving
//! paths from `data_root`) happens once, eagerly, in the orchestrator's
//! `normalize()` via [`SourceCommon::fold_defaults`] and
//! [`SourceCommon::resolve_paths`]. Downstream code receives a fully-resolved,
//! self-contained [`SourceCommon`] and never re-derives anything.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod download_params;
pub use download_params::DownloadParams;

/// Append a JSONL line per upsert into `<raw_path>/events/<table>.jsonl`.
/// Write-only mirror of the raw store, never read by the pipeline. See
/// `docs/dev/data_architecture_ingestion.md` § "Wire-event tape (JSONL)" — the
/// tape is intended to be always present so a human can `tail -f` the wire
/// payload off any source without opening doltlite.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventTapeConfig {
    /// Tape is on unless explicitly disabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for EventTapeConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The shared tunables every source carries, composed (not flattened) into each
/// provider's `*-config` crate as `common:`. After the orchestrator's
/// `normalize()` these hold fully-resolved values: [`Self::raw_path`] is always
/// `Some` (absolute), [`Self::input_path`] is tilde-expanded when set (and stays
/// `None` when omitted — its presence is load-bearing for "is this file-backed
/// source configured?"), and the knobs have the global [`Defaults`] folded in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceCommon {
    /// Where a file-backed source's export is read *from* (a `.mbox`, a `.vcf`,
    /// an unzipped Takeout root, …). `None` for API sources. Tilde-expanded by
    /// `normalize()`; never default-filled (its `Some`-ness signals a
    /// configured file-backed source).
    #[serde(default)]
    pub input_path: Option<PathBuf>,
    /// Where *we* keep this source's raw store (`entities.doltlite_db`,
    /// `blobs.doltlite_db`, the `events/` tape). Defaults to
    /// `<data_root>/<name>/raw`; `normalize()` fills this so it is always
    /// `Some` afterward.
    #[serde(default)]
    pub raw_path: Option<PathBuf>,
    /// Skip downloading any blob attachment larger than this many bytes.
    /// `None` = no limit. Consumed only by providers that download attachments.
    #[serde(default)]
    pub blob_size_limit_bytes: Option<u64>,
    /// Rate-limit give-up bounds for this source's download step.
    #[serde(default, alias = "extract_params")]
    pub download_params: DownloadParams,
    /// Wire-event tape config. `None` = enabled (the default).
    #[serde(default)]
    pub event_tape: Option<EventTapeConfig>,
}

/// Global base values for the propagatable [`SourceCommon`] knobs — the
/// top-level `defaults:` block. Pure authoring sugar: `normalize()` folds these
/// into every source's `common`, after which this block is spent and never read
/// again. Note it carries no paths (those derive from `data_root`/`name`, not
/// from a default).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub blob_size_limit_bytes: Option<u64>,
    #[serde(default, alias = "extract_params")]
    pub download_params: DownloadParams,
    #[serde(default)]
    pub event_tape: Option<EventTapeConfig>,
}

impl SourceCommon {
    /// Fold the global [`Defaults`] base under this source's own values: the
    /// source's `Some`/explicit value wins, an absent value falls through to
    /// the default. Idempotent; run once in `normalize()`.
    pub fn fold_defaults(&mut self, d: &Defaults) {
        self.blob_size_limit_bytes = self.blob_size_limit_bytes.or(d.blob_size_limit_bytes);
        // `merge(base, source)` lets the source win field-by-field.
        self.download_params = d.download_params.merge(&self.download_params);
        self.event_tape = self.event_tape.take().or_else(|| d.event_tape.clone());
    }

    /// Resolve paths against the (already tilde-expanded) `data_root` and the
    /// source's `name`. Fills [`Self::raw_path`] with the
    /// `<data_root>/<name>/raw` default when unset; tilde-expands an explicit
    /// `input_path` but leaves it `None` when omitted. Run once in
    /// `normalize()`.
    ///
    /// Layout: every artifact a stanza produces is grouped under
    /// `<data_root>/<name>/` (`raw/` here, `rendered_md/` on the render
    /// side), so a source's whole footprint is one self-contained subtree.
    pub fn resolve_paths(&mut self, data_root: &Path, name: &str) {
        let default_raw = data_root.join(name).join("raw");
        self.raw_path = Some(match self.raw_path.take() {
            Some(p) => expand_tilde(&p.display().to_string()),
            None => default_raw,
        });
        if let Some(p) = self.input_path.take() {
            self.input_path = Some(expand_tilde(&p.display().to_string()));
        }
    }

    /// Resolved raw-store directory. Valid only after [`Self::resolve_paths`].
    pub fn raw_path(&self) -> &Path {
        self.raw_path
            .as_deref()
            .expect("SourceCommon::raw_path read before normalize()")
    }

    /// Resolved input path: the explicit `input_path` if set, else the raw dir
    /// (the meaningless-but-harmless fallback for API sources). Valid only
    /// after [`Self::resolve_paths`].
    pub fn input_or_raw_path(&self) -> &Path {
        self.input_path
            .as_deref()
            .unwrap_or_else(|| self.raw_path())
    }

    /// Whether the wire-event tape is enabled (`None` → enabled default).
    pub fn event_tape_enabled(&self) -> bool {
        self.event_tape.as_ref().map(|e| e.enabled).unwrap_or(true)
    }
}

fn default_true() -> bool {
    true
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_defaults_source_wins_then_global_then_builtin() {
        let defaults = Defaults {
            blob_size_limit_bytes: Some(5_000_000),
            download_params: DownloadParams {
                maximum_time_without_progress_in_minutes: Some(30),
                maximum_sequential_failed_requests: Some(50),
            },
            event_tape: None,
        };

        // Source overrides one download knob + blob cap; the rest fall through.
        let mut common = SourceCommon {
            blob_size_limit_bytes: Some(1_000_000),
            download_params: DownloadParams {
                maximum_time_without_progress_in_minutes: None,
                maximum_sequential_failed_requests: Some(100),
            },
            ..Default::default()
        };
        common.fold_defaults(&defaults);

        assert_eq!(common.blob_size_limit_bytes, Some(1_000_000)); // source wins
        assert_eq!(
            common
                .download_params
                .maximum_time_without_progress_in_minutes,
            Some(30) // fell through to global
        );
        assert_eq!(
            common.download_params.maximum_sequential_failed_requests,
            Some(100) // source wins
        );
        assert!(common.event_tape_enabled()); // None → enabled
    }

    #[test]
    fn resolve_paths_defaults_raw_keeps_input_none() {
        let mut common = SourceCommon::default();
        common.resolve_paths(Path::new("/data"), "slack");
        assert_eq!(common.raw_path(), Path::new("/data/slack/raw"));
        // input_path stays None (load-bearing for is_managed); input_or_raw
        // then falls back to the raw dir for API sources.
        assert!(common.input_path.is_none());
        assert_eq!(common.input_or_raw_path(), Path::new("/data/slack/raw"));
    }

    #[test]
    fn resolve_paths_keeps_explicit_input() {
        let mut common = SourceCommon {
            input_path: Some(PathBuf::from("/exports/mail.mbox")),
            ..Default::default()
        };
        common.resolve_paths(Path::new("/data"), "gmail");
        assert_eq!(common.raw_path(), Path::new("/data/gmail/raw")); // still defaulted
        assert_eq!(common.input_or_raw_path(), Path::new("/exports/mail.mbox"));
    }
}
