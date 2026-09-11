//! Schema-only foundation crate shared by every provider `*-config`
//! crate.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod download_params;
pub mod glob;
pub mod probe;
pub use download_params::DownloadParams;
pub use glob::glob_match;
pub use probe::{ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport};

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
/// provider's `*-config` crate as `common:`. After `resolve_paths()`
/// [`Self::raw_path`] is always `Some` (absolute), and the knobs have the
/// global [`Defaults`] folded in. Where a file-backed source reads *from*
/// is not here: that is the `path` of the method table that reads it
/// (`[steps.params.export] path = …`), declared per provider. Strict: a
/// misspelled knob is refused, not ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCommon {
    /// Where *we* keep this source's raw store (`entities.doltlite_db`,
    /// `blobs.doltlite_db`, the `events/` tape): the tree the ingest step
    /// writes, `<data_root>/<group>/ingest`, filled by `resolve_paths`.
    /// Not a config key — the runner versions and every consumer reads
    /// the tree by its id, so a store kept elsewhere is a symlink at the
    /// tree.
    #[serde(skip)]
    pub raw_path: Option<PathBuf>,
    /// Skip downloading any blob attachment larger than this many bytes.
    /// `None` = no limit. Consumed only by providers that download attachments.
    #[serde(default)]
    pub blob_size_limit_bytes: Option<u64>,
    /// Wipe this source's entity tables and resume cursors before every
    /// ingest, so the run rewrites them from what the input holds now and
    /// anything the input has dropped falls out. Set it for a source whose
    /// input is a *complete* snapshot (a Takeout export, a phone backup, a
    /// `.vcf` directory) — that is the only case where absence means
    /// deletion. Leave it off for an input that is itself a partial or
    /// evicting cache, where absence means "not cached here" and a wipe
    /// would destroy real history.
    ///
    /// Only entity tables go; the blob CAS keeps its bytes (a re-ingest
    /// re-registers the ones still referenced, and the rest await a
    /// collector we have not built). The old rows stay in doltlite history,
    /// so `dolt_diff` still says what the input lost.
    #[serde(default)]
    pub always_clear_before_ingest: bool,
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

    /// Fills [`Self::raw_path`] with the tree the ingest step writes,
    /// `<data_root>/<group>/ingest`. Run once, before anything reads it.
    pub fn resolve_paths(&mut self, raw: PathBuf) {
        self.raw_path = Some(raw);
    }

    pub fn raw_path(&self) -> &Path {
        self.raw_path
            .as_deref()
            .expect("SourceCommon::raw_path read before resolve_paths()")
    }

    pub fn event_tape_enabled(&self) -> bool {
        self.event_tape.as_ref().map(|e| e.enabled).unwrap_or(true)
    }
}

/// Per-source latchkey knobs, for a source whose downloader authenticates
/// through the `latchkey` CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatchkeySettings {
    /// Which stored latchkey account this source mirrors
    /// (`latchkey --account <acct> curl …`).
    #[serde(default)]
    pub account: Option<String>,
}

impl LatchkeySettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.account.as_ref().is_some_and(|a| a.trim().is_empty()) {
            return Err(
                "`latchkey_settings.account` names a stored latchkey account; omit it entirely \
                 to use the only one (`latchkey auth list` shows what is stored)"
                    .to_string(),
            );
        }
        Ok(())
    }

    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }
}

/// The slim per-source envelope for the **render** wave. Render's only
/// shared tunables are where the raw store lives ([`Self::raw_path`])
/// and — for file-tree-backed sources like perseus that render straight
/// from a staged tree — [`Self::input_path`]. Download-side knobs
/// (blob limits, rate limits, event tape) deliberately don't exist
/// here, so a render step's params can't smuggle them in. Strict
/// (`deny_unknown_fields`): a download-shaped params blob on a render
/// step fails loudly instead of being silently ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderCommon {
    /// A tree staged by hand for a render that reads files rather than
    /// a raw store. Only perseus reads it; every other render reads
    /// [`Self::raw_path`].
    #[serde(default)]
    pub input_path: Option<PathBuf>,
    /// The raw store this render reads — the tree its ingest input
    /// names, filled by `resolve_paths`. Not a config key, for the same
    /// reason as [`SourceCommon::raw_path`].
    #[serde(skip)]
    pub raw_path: Option<PathBuf>,
}

impl RenderCommon {
    pub fn resolve_paths(&mut self, raw: PathBuf) {
        self.raw_path = Some(raw);
        if let Some(p) = self.input_path.take() {
            self.input_path = Some(expand_tilde(&p));
        }
    }

    pub fn raw_path(&self) -> &Path {
        self.raw_path
            .as_deref()
            .expect("RenderCommon::raw_path read before resolve_paths()")
    }

    pub fn input_or_raw_path(&self) -> &Path {
        self.input_path
            .as_deref()
            .unwrap_or_else(|| self.raw_path())
    }
}

/// Whether an ingest method reaches a live service or reads what is
/// already on this machine. Every method table a provider accepts
/// declares one ([`IngestMethods`]), and a step's reach is read off its
/// written params against that list — so the Manage row's "Download" /
/// "Import", the wizard's credentials section and
/// `DATALIB_DAG_RESET_AND_REDOWNLOAD` all answer from one place.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Reach {
    /// Fetches from a live origin: an HTTP API, a JMAP or CardDAV server.
    Origin,
    /// Reads files already on disk: an export, a backup, a folder.
    Local,
}

impl Reach {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Reach> {
        s.parse().ok()
    }
}

/// One way an ingest step's params can say where its data comes from:
/// a dotted path into the params (`api`, `gmail`, `export`) and
/// what holding it means. A method is *held* when the path is written
/// and its value is neither `null` nor `false`, so a table counts by
/// presence (`api = {}` is a complete selection) and a flag such as
/// linkedin's `export.fetch_photos` only when on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IngestMethod {
    pub path: &'static str,
    pub reach: Reach,
}

impl IngestMethod {
    pub const fn origin(path: &'static str) -> IngestMethod {
        IngestMethod {
            path,
            reach: Reach::Origin,
        }
    }

    pub const fn local(path: &'static str) -> IngestMethod {
        IngestMethod {
            path,
            reach: Reach::Local,
        }
    }
}

/// The methods a provider's ingest config accepts, declared by the one
/// crate that knows the answer. Every `<P>Config` implements it;
/// `datalib-step` refuses an `ingest` step that holds none of them, and
/// the UI reads the same lists through a golden generated from them
/// (`datalib/ui/src/config/ingestMethods.json`).
pub trait IngestMethods {
    const METHODS: &'static [IngestMethod];
}

/// The one field every method that reads files on disk needs, for the
/// providers whose method needs nothing else: `[steps.params.export]
/// path = "~/Downloads/export"`. Tilde-expanded on read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPath {
    pub path: PathBuf,
}

impl LocalPath {
    pub fn path(&self) -> PathBuf {
        expand_tilde(&self.path)
    }
}

/// Render config for providers with no render-specific knobs — just the
/// shared envelope. Provider config crates alias this as their
/// `<P>RenderConfig` so every provider exposes the same per-phase pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BareRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
}

fn default_true() -> bool {
    true
}

/// `~/x` → `$HOME/x`; anything else unchanged. Every path a config
/// names goes through this once, so a provider's method table calls it
/// on its own `path`.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.display().to_string();
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
    use strum::VariantArray;

    /// Omitting the block is the "only stored account" case and must stay
    /// the zero-config default -- every source that predates it relies on
    /// deserializing from nothing at all.
    #[test]
    fn latchkey_settings_default_to_no_account() {
        let s: LatchkeySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.account(), None);
        assert!(s.validate().is_ok());
        assert_eq!(LatchkeySettings::default().account(), None);
    }

    /// The unnamed default account is addressed by *omitting* the field.
    /// An empty string would reach latchkey as `--account ""`, which is a
    /// different (and almost never intended) request.
    #[test]
    fn latchkey_settings_reject_a_blank_account() {
        for blank in ["", " ", "\t"] {
            let s = LatchkeySettings {
                account: Some(blank.to_string()),
            };
            let err = s
                .validate()
                .expect_err("a blank account must not reach latchkey");
            assert!(err.contains("latchkey_settings.account"), "{err}");
        }
    }

    /// A typo inside the block is a config error, not a silently ignored
    /// key -- otherwise `acount = "..."` would mirror the wrong identity.
    #[test]
    fn latchkey_settings_reject_an_unknown_key() {
        assert!(serde_json::from_str::<LatchkeySettings>(r#"{"acount": "me"}"#).is_err());
    }

    #[test]
    fn latchkey_settings_round_trip_a_named_account() {
        let s: LatchkeySettings = serde_json::from_str(r#"{"account": "thad@imbue.com"}"#).unwrap();
        assert_eq!(s.account(), Some("thad@imbue.com"));
        assert!(s.validate().is_ok());
    }

    /// `Reach` derives both strum and serde; the two spell the variants
    /// independently, so their agreement is a real check.
    #[test]
    fn reach_spellings_agree_between_strum_and_serde() {
        for &r in Reach::VARIANTS {
            let via_serde = serde_json::to_value(r).unwrap();
            assert_eq!(via_serde.as_str(), Some(r.as_str()));
            assert_eq!(Reach::parse(r.as_str()), Some(r));
        }
        assert_eq!(Reach::parse("remote"), None);
    }

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
    fn resolve_paths_sets_raw() {
        let mut common = SourceCommon::default();
        common.resolve_paths(PathBuf::from("/data/slack/ingest"));
        assert_eq!(common.raw_path(), Path::new("/data/slack/ingest"));
    }

    /// `common.input_path` was where every file-backed source read from
    /// before each method table carried its own `path`, and
    /// `common.raw_path` named a store the step then refused unless it was
    /// its own tree. Both are gone from the envelope rather than ignored,
    /// so a config still writing either is refused (and `datalib-step`
    /// names the migrator) instead of quietly reading the wrong tree.
    #[test]
    fn the_envelope_has_no_path_keys() {
        let v: serde_json::Value = serde_json::to_value(SourceCommon::default()).unwrap();
        assert!(v.get("input_path").is_none(), "{v}");
        assert!(v.get("raw_path").is_none(), "{v}");
        for key in ["input_path", "raw_path", "blob_size_limit"] {
            let err = serde_json::from_value::<SourceCommon>(serde_json::json!({ key: "/x" }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("unknown field"), "{key}: {err}");
        }
        let r: RenderCommon = serde_json::from_str("{}").unwrap();
        assert!(r.raw_path.is_none());
        assert!(serde_json::from_str::<RenderCommon>(r#"{"raw_path": "/x"}"#).is_err());
    }

    #[test]
    fn a_local_path_expands_a_tilde() {
        let t: LocalPath = serde_json::from_str(r#"{"path": "~/exports/x"}"#).unwrap();
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(t.path(), Path::new(&home).join("exports/x"));
        }
        let t: LocalPath = serde_json::from_str(r#"{"path": "/abs/x"}"#).unwrap();
        assert_eq!(t.path(), Path::new("/abs/x"));
        assert!(serde_json::from_str::<LocalPath>(r#"{"pth": "/x"}"#).is_err());
    }
}
