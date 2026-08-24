//! Provider-owned config schema for the `pdf` source. Schema-only
//! (serde + anyhow), so the orchestrator can name [`PdfConfig`] without
//! linking the provider.
//!
//! `pdf` is purely file-backed: there is no API and no `sync:` block —
//! it scans the tree at `common.input_path` for PDFs, converts the ones
//! that carry real text, and indexes the result. Provider knobs are the
//! ignore cascade, a size ceiling, and the OCR switch (off, and
//! currently only off — see [`PdfConfig::ocr`]).

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The pdf-owned slice of a `pdf` source. The scan root is
/// `common.input_path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`. The scanned tree is
    /// `input_path`.
    #[serde(default)]
    pub common: SourceCommon,

    /// Gitignore-shaped patterns pruned from the scan, in addition to
    /// any `.gitignore` files found in the tree. Matched by the
    /// `ignore` crate (the one ripgrep uses), so `**`, anchored `/`,
    /// and character classes all behave as expected.
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Skip files larger than this. A multi-gigabyte PDF is nearly
    /// always a scanned book or a corrupt file, and either way we do
    /// not want one document to stall a whole scan. `None` means no
    /// ceiling.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: Option<u64>,

    /// Run OCR over pages that carry no extractable text.
    ///
    /// **Not implemented yet; setting it `true` is rejected at load
    /// time** rather than silently ignored, so a config that asks for
    /// OCR fails loudly instead of quietly indexing nothing for every
    /// scanned document. The first pass classifies scanned PDFs and
    /// records them (`pdf_documents.needs_ocr`) without converting
    /// them, which is what makes adding an engine later a pure
    /// addition: the rows that need one are already enumerated.
    #[serde(default)]
    pub ocr: bool,
}

/// 512 MiB. Comfortably above any real document, well below the size
/// where hashing and conversion stop being background-cheap.
fn default_max_bytes() -> Option<u64> {
    Some(512 * 1024 * 1024)
}

impl PdfConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.ocr {
            anyhow::bail!(
                "pdf: `ocr = true` is not supported yet — this build classifies scanned \
                 PDFs and records them in `pdf_documents.needs_ocr` without converting \
                 them. Remove the key (or set it false) to proceed."
            );
        }
        if let Some(0) = self.max_bytes {
            anyhow::bail!("pdf: `max_bytes = 0` would skip every file; omit it for no limit");
        }
        Ok(())
    }
}

/// Params for the render step. Render re-reads the raw store and needs
/// no provider-specific knobs of its own, so this is the shared bare
/// envelope (see the per-phase params split).
pub type PdfRenderConfig = datalib_source_common::BareRenderConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        PdfConfig::default().validate().unwrap();
    }

    #[test]
    fn ocr_true_is_rejected_loudly() {
        let c = PdfConfig {
            ocr: true,
            ..Default::default()
        };
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("not supported yet"), "{err}");
    }

    #[test]
    fn zero_max_bytes_is_rejected() {
        let c = PdfConfig {
            max_bytes: Some(0),
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = toml::from_str::<PdfConfig>("ocrr = true").unwrap_err();
        assert!(e.to_string().contains("ocrr"), "{e}");
    }

    #[test]
    fn max_bytes_defaults_to_512mib() {
        let c: PdfConfig = toml::from_str("").unwrap();
        assert_eq!(c.max_bytes, Some(512 * 1024 * 1024));
    }
}
