//! HTTP playback synthesizer for the LinkedIn connection-photo fetch.
//!
//! LinkedIn extract is otherwise file-backed (it walks CSVs), but the
//! optional photo fetch ([`crate::extract::photos`]) makes real web
//! requests: GET the profile page, scrape `og:image`, GET the image. To
//! exercise that path hermetically in the TNG fixture pipeline, this
//! synthesizer reads `Connections.csv` and writes, for each connection:
//!
//!   1. a profile-page fixture whose HTML carries an `og:image` meta tag
//!      pointing at a synthetic media URL, and
//!   2. an image fixture at that media URL with placeholder bytes.
//!
//! The requests are built with the same [`HttpRequest::get(..).plain()`]
//! the extractor issues, so [`write_fixture`]'s key matches the lookup
//! at replay time. Fully synthetic — no real LinkedIn bytes committed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::http::{HttpRequest, HttpResponse};
use frankweiler_etl::synthesize::{write_fixture, SynthesizeReport, Synthesizer};

use crate::extract::strip_notes_preamble;

pub struct LinkedinSynth {
    /// The unzipped export directory (holds `Connections.csv`).
    export_dir: PathBuf,
}

impl LinkedinSynth {
    pub fn new(export_dir: impl Into<PathBuf>) -> Self {
        Self {
            export_dir: export_dir.into(),
        }
    }
}

impl Synthesizer for LinkedinSynth {
    fn name(&self) -> &'static str {
        "linkedin"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let mut report = SynthesizeReport::default();
        let urls = connection_urls(&self.export_dir)?;
        for url in urls {
            let img_url = synthetic_image_url(&url);

            // 1) profile page → og:image
            let page = HttpResponse {
                status: 200,
                headers: html_headers(),
                body: profile_html(&img_url).into_bytes(),
                duration_ms: 0,
            };
            write_fixture(out_root, &HttpRequest::get("linkedin", &url).plain(), &page)?;

            // 2) the image bytes (placeholder — content is irrelevant; it
            //    lands in CAS and renders as a blob file).
            let img = HttpResponse {
                status: 200,
                headers: png_headers(),
                body: format!("FAKE-PNG bytes for {url}").into_bytes(),
                duration_ms: 0,
            };
            write_fixture(
                out_root,
                &HttpRequest::get("linkedin", &img_url).plain(),
                &img,
            )?;

            report.fixtures_written += 2;
        }
        Ok(report)
    }
}

/// Synthetic, deterministic media URL for a profile — the `og:image` the
/// page fixture advertises and the image fixture answers to.
fn synthetic_image_url(profile_url: &str) -> String {
    let slug = profile_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("anon");
    format!("https://media.licdn.example/fixture/{slug}.png")
}

fn profile_html(img_url: &str) -> String {
    format!(
        "<!doctype html><html><head>\
         <meta property=\"og:image\" content=\"{img_url}\">\
         </head><body>profile</body></html>"
    )
}

fn html_headers() -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert("content-type".into(), "text/html; charset=utf-8".into());
    h
}

fn png_headers() -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert("content-type".into(), "image/png".into());
    h
}

/// Read the `URL` column of `Connections.csv` (tolerating the `Notes:`
/// preamble). Empty when the file is absent or has no URL column.
fn connection_urls(export_dir: &Path) -> Result<Vec<String>> {
    let path = export_dir.join("Connections.csv");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let body = strip_notes_preamble(&raw);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context("read Connections.csv header")?
        .clone();
    let Some(url_col) = headers.iter().position(|h| h.trim() == "URL") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.context("read Connections.csv record")?;
        if let Some(u) = rec.get(url_col) {
            let u = u.trim();
            if !u.is_empty() {
                out.push(u.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_image_url_uses_profile_slug() {
        assert_eq!(
            synthetic_image_url("https://www.linkedin.com/in/jlp"),
            "https://media.licdn.example/fixture/jlp.png"
        );
    }

    #[test]
    fn profile_html_advertises_og_image() {
        let html = profile_html("https://m/p.png");
        assert!(html.contains("og:image"));
        assert!(html.contains("https://m/p.png"));
    }

    #[test]
    fn reads_connection_urls_with_notes_preamble() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Connections.csv"),
            "Notes:\n\"blah\"\n\nFirst Name,Last Name,URL,Company\nA,B,https://x/in/a,Co\n",
        )
        .unwrap();
        let urls = connection_urls(dir.path()).unwrap();
        assert_eq!(urls, vec!["https://x/in/a"]);
    }
}
