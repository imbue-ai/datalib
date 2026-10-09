//! Garmin Connect API transport: every request goes through
//! [`datalib_etl_web::http::latchkey_curl`]. latchkey's Garmin plugin holds
//! the credential and mints the hourly bearer from it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl::events;
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};

pub const TIMEOUT: Duration = Duration::from_secs(120);

/// What the phone app sends; also what every playback fixture is keyed
/// under, so a synthesizer must build its requests with [`req_get`].
pub const USER_AGENT: &str = "GCM-iOS-5.22.1.4";

/// The one Garmin the plugin reaches; garmin.cn is a separate service.
pub const DOMAIN: &str = "garmin.com";

pub fn base_url(domain: &str) -> String {
    format!("https://connectapi.{domain}")
}

/// The request shape shared by the client and the fixture synthesizer.
pub fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Garmin, url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .timeout(TIMEOUT)
}

/// A request for bytes rather than JSON (FIT files, zips).
pub fn req_get_bytes(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Garmin, url)
        .header("User-Agent", USER_AGENT)
        .timeout(TIMEOUT)
}

#[derive(thiserror::Error, Debug)]
pub enum GarminError {
    /// 401/403: Garmin refused the credential latchkey sent; or latchkey
    /// would not send one at all, which every later request would share.
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

/// Shared by every loop of a run, so a request takes `&self`.
pub struct GarminClient {
    latchkey: LatchkeySettings,
    base: String,
    requests: AtomicU64,
}

impl GarminClient {
    pub fn new(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            base: base_url(DOMAIN),
            requests: AtomicU64::new(0),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    async fn send(&self, build: fn(&str) -> HttpRequest, path: &str) -> Result<(u16, Vec<u8>)> {
        let url = self.url(path);
        let req = build(&url).latchkey(self.latchkey.clone());
        let resp = latchkey_curl(&req)
            .await
            .map_err(|e| transport_error(e, path))?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
        Ok((resp.status, resp.body))
    }

    /// `GET` a JSON endpoint. A 204, a 404 or an empty body is
    /// [`Fetched::Nothing`] — Garmin answers all three for a day that
    /// has no data, depending on the service.
    pub async fn get_json(&self, path: &str) -> Result<Fetched<Value>> {
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
    pub async fn get_bytes(&self, path: &str) -> Result<Fetched<Vec<u8>>> {
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

/// latchkey exits 1 when it will not send the request at all: no
/// credential, or the plugin could not mint a bearer. curl's own failures
/// (DNS, a refused connection, a timeout) have codes of their own.
fn transport_error(e: HttpError, path: &str) -> GarminError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            GarminError::Auth(format!("GET {path}: {e}"))
        }
        other => GarminError::Permanent(other.to_string()),
    }
}

fn preview(body: &[u8]) -> String {
    String::from_utf8_lossy(body).chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curl_exit(exit: i32) -> HttpError {
        HttpError::Curl {
            service: HttpService::Garmin,
            url: "https://connectapi.garmin.com/hrv-service/hrv/2369-04-14".into(),
            exit,
            stderr: "Error: No credentials found for garmin.".into(),
        }
    }

    /// A request latchkey would not send (no credential, or the bearer
    /// exchange failed) must end the run, not record every day of every
    /// metric as failed before giving up.
    #[test]
    fn a_request_latchkey_will_not_send_is_an_auth_failure() {
        let e = transport_error(curl_exit(1), "/hrv-service/hrv/2369-04-14");
        assert!(matches!(e, GarminError::Auth(_)), "{e}");
    }

    #[test]
    fn a_network_failure_is_not_an_auth_failure() {
        let e = transport_error(curl_exit(7), "/hrv-service/hrv/2369-04-14");
        assert!(matches!(e, GarminError::Permanent(_)), "{e}");
    }
}
