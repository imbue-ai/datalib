//! Google Drive HTTP fixture synthesizer: one JSON spec (the TNG
//! account's files) → every playback response a first sync asks for
//! with the default kinds and no exports. The request shapes are the
//! ingest's own (`ingest::api`), so a URL change there cannot strand
//! the fixtures.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl_google_drive_config::GoogleDriveApi;
use datalib_etl_web::http::HttpResponse;
use datalib_etl_web::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ingest::api;

/// The spec file: the files the account can see, each with everything
/// a capture reads.
#[derive(Debug, Deserialize)]
pub struct DriveSpec {
    pub files: Vec<SpecFile>,
}

#[derive(Debug, Deserialize)]
pub struct SpecFile {
    /// The listing entry, as `files.list` returns it.
    pub entry: Value,
    /// The native API's answer, image links included.
    pub native: Value,
    #[serde(default)]
    pub comments: Vec<Value>,
    #[serde(default)]
    pub revisions: Vec<Value>,
    /// Image link in `native` → a file beside the spec holding its bytes.
    #[serde(default)]
    pub images: BTreeMap<String, String>,
}

pub struct GoogleDriveSynth {
    pub spec_path: PathBuf,
}

impl GoogleDriveSynth {
    pub fn new(spec_path: impl Into<PathBuf>) -> Self {
        Self {
            spec_path: spec_path.into(),
        }
    }
}

impl Synthesizer for GoogleDriveSynth {
    fn name(&self) -> &'static str {
        "google_drive"
    }

    fn synthesize(&self, out_root: &std::path::Path) -> Result<SynthesizeReport> {
        let bytes = fs::read(&self.spec_path)
            .with_context(|| format!("read {}", self.spec_path.display()))?;
        let spec: DriveSpec = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", self.spec_path.display()))?;
        let beside = self.spec_path.parent().unwrap_or(std::path::Path::new("."));
        let config = GoogleDriveApi::default();
        let mut n = 0usize;
        let mut put = |req, resp| -> Result<()> {
            write_fixture(out_root, &req, &resp)?;
            n += 1;
            Ok(())
        };

        let entries: Vec<&Value> = spec.files.iter().map(|f| &f.entry).collect();
        put(
            api::req_json(&api::list_url(&config.kinds, config.owned_only, None)),
            json_response(&json!({"kind": "drive#fileList", "incompleteSearch": false,
                                  "files": entries})),
        )?;
        for file in &spec.files {
            let id = file.entry["id"].as_str().context("a file with no id")?;
            let mime = file.entry["mimeType"].as_str().unwrap_or("");
            let kind = api::kind_of_mime(mime)
                .with_context(|| format!("{id}: {mime:?} is not a kind Drive mirrors"))?;
            put(
                api::req_json(&api::revisions_url(id, None)),
                json_response(&json!({"kind": "drive#revisionList",
                                      "revisions": file.revisions})),
            )?;
            put(
                api::req_json(&api::comments_url(id, None)),
                json_response(&json!({"kind": "drive#commentList",
                                      "comments": file.comments})),
            )?;
            put(
                api::req_bytes(&api::native_url(kind, id)),
                json_response(&file.native),
            )?;
            for (url, name) in &file.images {
                let path = beside.join(name);
                let body = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                put(
                    api::req_image(url),
                    HttpResponse {
                        status: 200,
                        headers: BTreeMap::from([(
                            "content-type".to_string(),
                            "image/png".to_string(),
                        )]),
                        body,
                        duration_ms: 0,
                    },
                )?;
            }
        }
        Ok(SynthesizeReport {
            fixtures_written: n,
        })
    }
}
