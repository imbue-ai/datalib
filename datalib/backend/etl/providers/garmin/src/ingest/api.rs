//! Garmin Connect API transport: every request goes through
//! [`datalib_etl::http::latchkey_curl`] for its retry policy and
//! playback, but bypasses the latchkey shim — the bearer comes from
//! [`crate::auth::Credentials`] and rides on [`HttpRequest::bearer`].

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl::events;
use datalib_etl::http::{latchkey_curl, HttpRequest, HttpService};

use crate::auth::Credentials;

pub const TIMEOUT: Duration = Duration::from_secs(120);

/// What the phone app sends; also what every playback fixture is keyed
/// under, so a synthesizer must build its requests with [`req_get`].
pub const USER_AGENT: &str = "GCM-iOS-5.22.1.4";

pub fn base_url(domain: &str) -> String {
    format!("https://connectapi.{domain}")
}

/// The request shape shared by the client and the fixture synthesizer.
pub fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Garmin, url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .plain()
        .timeout(TIMEOUT)
}

/// A request for bytes rather than JSON (FIT files, zips).
pub fn req_get_bytes(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Garmin, url)
        .header("User-Agent", USER_AGENT)
        .plain()
        .timeout(TIMEOUT)
}

#[derive(thiserror::Error, Debug)]
pub enum GarminError {
    /// 401/403: the bearer was refused even after one refresh.
    #[error("unauthorized: {0}")]
    Auth(String),
    #[error("{0}")]
    Permanent(String),
}

/// One response, with "there is nothing for that day" folded to `None`.
pub enum Fetched<T> {
    Some(T),
    Nothing,
}

pub struct GarminClient {
    creds: Credentials,
    base: String,
    pub requests: u64,
    pub network_seconds: f64,
}

impl GarminClient {
    pub fn new(creds: Credentials) -> Self {
        let base = base_url(creds.domain());
        Self {
            creds,
            base,
            requests: 0,
            network_seconds: 0.0,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn send(&mut self, build: fn(&str) -> HttpRequest, path: &str) -> Result<(u16, Vec<u8>)> {
        let url = self.url(path);
        let mut refreshed = false;
        loop {
            let bearer = self.creds.bearer().await?;
            let req = build(&url).bearer(bearer);
            let resp = latchkey_curl(&req)
                .await
                .map_err(|e| GarminError::Permanent(e.to_string()))?;
            self.network_seconds += (resp.duration_ms as f64) / 1000.0;
            self.requests += 1;
            if resp.status == 401 && !refreshed {
                // The cached bearer can be revoked before its stated
                // expiry; one forced refresh settles whether it is the
                // token or the account.
                self.creds.force_refresh();
                refreshed = true;
                continue;
            }
            events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
            return Ok((resp.status, resp.body));
        }
    }

    /// `GET` a JSON endpoint. A 204, a 404 or an empty body is
    /// [`Fetched::Nothing`] — Garmin answers all three for a day that
    /// has no data, depending on the service.
    pub async fn get_json(&mut self, path: &str) -> Result<Fetched<Value>> {
        let (status, body) = self.send(req_get, path).await?;
        match status {
            200 if body.is_empty() => Ok(Fetched::Nothing),
            200 => {
                let value: Value = serde_json::from_slice(&body).map_err(|e| {
                    let preview: String =
                        String::from_utf8_lossy(&body).chars().take(200).collect();
                    anyhow!("GET {path}: invalid JSON: {e}; body[:200]={preview:?}")
                })?;
                Ok(Fetched::Some(value))
            }
            204 | 404 => Ok(Fetched::Nothing),
            401 | 403 => Err(GarminError::Auth(format!(
                "GET {path} -> HTTP {status}: {}",
                preview(&body)
            ))
            .into()),
            _ => bail!("GET {path} -> HTTP {status}: {}", preview(&body)),
        }
    }

    /// `GET` a binary endpoint. 404 is [`Fetched::Nothing`]: an activity
    /// created by hand on the website has no FIT file.
    pub async fn get_bytes(&mut self, path: &str) -> Result<Fetched<Vec<u8>>> {
        let (status, body) = self.send(req_get_bytes, path).await?;
        match status {
            200 => Ok(Fetched::Some(body)),
            204 | 404 => Ok(Fetched::Nothing),
            401 | 403 => Err(GarminError::Auth(format!(
                "GET {path} -> HTTP {status}: {}",
                preview(&body)
            ))
            .into()),
            _ => bail!("GET {path} -> HTTP {status}: {}", preview(&body)),
        }
    }
}

fn preview(body: &[u8]) -> String {
    String::from_utf8_lossy(body).chars().take(300).collect()
}
