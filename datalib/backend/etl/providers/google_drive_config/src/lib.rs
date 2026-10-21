//! Config schema for the `google_drive` source: a Google account's Docs,
//! Sheets and Slides, mirrored with Google's native representation of
//! each and the comments, revisions and exports beside it. Schema only
//! (serde + anyhow), so anything that needs to understand a config can
//! link this without the downloader. `api` is its one way in.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// The google_drive-owned slice of a `google_drive` source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDriveConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors, forwarded whole to
    /// the download client — see [`LatchkeySettings`]. The same account
    /// has to be signed in to latchkey's `google-drive`, `google-docs`,
    /// `google-sheets` and `google-slides` services.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    #[serde(default)]
    pub api: Option<GoogleDriveApi>,
}

impl GoogleDriveConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            api.validate()?;
        }
        Ok(())
    }
}

/// The live-API method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDriveApi {
    /// Which kinds of file to mirror. Default: all three.
    #[serde(default = "all_kinds")]
    pub kinds: Vec<FileKind>,
    /// Which converted copies to keep beside the native JSON, each for
    /// every version captured. Default: `odf` (`.odt`, `.ods`, `.odp`),
    /// which LibreOffice opens. Each costs a request and roughly 1–4 MB
    /// per version, since Google embeds the fonts.
    #[serde(default = "default_exports")]
    pub exports: Vec<ExportFormat>,
    /// Only the files the account owns, not the ones shared with it.
    #[serde(default)]
    pub owned_only: bool,
    /// When non-empty, mirror exactly these files: a bare file id or a
    /// pasted `https://docs.google.com/<kind>/d/<id>/…` link.
    #[serde(default)]
    pub file_ids: Vec<String>,
}

impl Default for GoogleDriveApi {
    fn default() -> Self {
        Self {
            kinds: all_kinds(),
            exports: default_exports(),
            owned_only: false,
            file_ids: Vec::new(),
        }
    }
}

impl GoogleDriveApi {
    pub fn validate(&self) -> anyhow::Result<()> {
        for given in &self.file_ids {
            if file_id_of(given).is_none() {
                anyhow::bail!(
                    "google_drive: {given:?} in `file_ids` is neither a file id nor a \
                     docs.google.com link with one in it"
                );
            }
        }
        Ok(())
    }

    /// `file_ids` as bare ids, links taken apart. An entry
    /// [`Self::validate`] refuses is left out.
    pub fn wanted_file_ids(&self) -> Vec<String> {
        self.file_ids
            .iter()
            .filter_map(|s| file_id_of(s))
            .map(str::to_string)
            .collect()
    }
}

/// The id in a bare file id or a `…/d/<id>/…` link. Drive ids are
/// letters, digits, `-` and `_`.
pub fn file_id_of(given: &str) -> Option<&str> {
    let given = given.trim();
    let id = match given.split_once("/d/") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None => given,
    };
    let looks_like_an_id = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    looks_like_an_id.then_some(id)
}

/// A kind of Google-native file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Document,
    Spreadsheet,
    Presentation,
}

/// A family of converted copy Drive's export makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// OpenDocument: `.odt`, `.ods`, `.odp`.
    Odf,
    /// Office Open XML: `.docx`, `.xlsx`, `.pptx`.
    Ooxml,
    Pdf,
}

fn all_kinds() -> Vec<FileKind> {
    vec![
        FileKind::Document,
        FileKind::Spreadsheet,
        FileKind::Presentation,
    ]
}

fn default_exports() -> Vec<ExportFormat> {
    vec![ExportFormat::Odf]
}

/// Params for the render step: there is no render step yet, but
/// `ingest_only!` still parses one.
pub type GoogleDriveRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GoogleDriveConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serde defaults and the `Default` impl have to agree, or a
    /// config that omits a knob behaves differently from one built in
    /// Rust.
    #[test]
    fn defaults_agree_both_ways() {
        let from_toml: GoogleDriveApi = toml::from_str("").unwrap();
        let built = GoogleDriveApi::default();
        assert_eq!(from_toml.kinds, built.kinds);
        assert_eq!(from_toml.exports, built.exports);
        assert_eq!(from_toml.exports, vec![ExportFormat::Odf]);
        assert_eq!(from_toml.kinds.len(), 3);
        assert!(!from_toml.owned_only);
    }

    #[test]
    fn knobs_parse() {
        let c: GoogleDriveApi = toml::from_str(
            r#"kinds = ["document"]
               exports = ["odf", "ooxml", "pdf"]
               owned_only = true"#,
        )
        .unwrap();
        assert_eq!(c.kinds, vec![FileKind::Document]);
        assert_eq!(c.exports.len(), 3);
        assert!(c.owned_only);
    }

    #[test]
    fn file_ids_take_bare_ids_and_links() {
        assert_eq!(file_id_of("1AbC-d_9"), Some("1AbC-d_9"));
        assert_eq!(
            file_id_of("https://docs.google.com/document/d/1AbC-d_9/edit?tab=t.0"),
            Some("1AbC-d_9")
        );
        assert_eq!(
            file_id_of("https://docs.google.com/spreadsheets/d/1AbC/edit#gid=0"),
            Some("1AbC")
        );
        assert_eq!(file_id_of("https://example.com/no-id-here"), None);
        assert_eq!(file_id_of(""), None);
    }

    #[test]
    fn a_file_id_that_is_not_one_is_refused() {
        let c: GoogleDriveConfig = toml::from_str("[api]\nfile_ids = [\"not an id\"]\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("not an id"), "{err}");
    }
}
