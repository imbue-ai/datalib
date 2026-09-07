//! Shared infrastructure for **HTTP fixture synthesizers**.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::http::{fixture_key, HttpRequest, HttpResponse};

/// One provider's synthesizer. Each impl knows how to walk its own
/// event-store layout (Notion's three entities, GitHub's PR snapshots,
/// Slack's per-channel JSONL, etc.) and emit the fixture files the
/// provider's Download step would otherwise fetch from the live API.
pub trait Synthesizer {
    fn name(&self) -> &'static str;

    /// Write every fixture this provider's Download step would need into
    /// `out_root`. Implementations should be idempotent — repeated runs
    /// overwrite the same files byte-identically.
    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport>;
}

#[derive(Debug, Default, Clone)]
pub struct SynthesizeReport {
    pub fixtures_written: usize,
}

/// Write a single playback fixture. The path is derived deterministically
/// from `req` (same hash the [`crate::http::latchkey_curl`] playback
/// branch looks up), so caller and replayer never have to agree on a
/// naming scheme separately.
pub fn write_fixture(out_root: &Path, req: &HttpRequest, resp: &HttpResponse) -> Result<()> {
    let dir = out_root.join(req.service.as_str());
    fs::create_dir_all(&dir).with_context(|| format!("create fixture dir {}", dir.display()))?;
    let path = dir.join(fixture_key(req));
    let bytes = serde_json::to_vec_pretty(resp).context("serialize HttpResponse")?;
    fs::write(&path, bytes).with_context(|| format!("write fixture {}", path.display()))?;
    Ok(())
}

/// Build a 200-OK `application/json` response from a JSON value. Most
/// API endpoints these synthesizers cover return JSON, so this is the
/// common case; for non-JSON bodies (Slack file-download CDN, GitHub
/// raw content), construct `HttpResponse` directly.
pub fn json_response(body: &Value) -> HttpResponse {
    let body_bytes = serde_json::to_vec(body).expect("Value always serializable");
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), "application/json".into());
    HttpResponse {
        status: 200,
        headers,
        body: body_bytes,
        duration_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpService;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn write_fixture_roundtrips_through_playback_key() {
        let d = tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Test, "https://example.com/v1/things?b=2&a=1");
        let resp = json_response(&json!({"hello": "world"}));
        write_fixture(d.path(), &req, &resp).unwrap();

        let key = fixture_key(&req);
        let path = d.path().join("test_provider").join(&key);
        assert!(path.exists(), "expected fixture at {}", path.display());

        let loaded: HttpResponse = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.status, 200);
        assert_eq!(loaded.body_str(), r#"{"hello":"world"}"#);
    }
}
