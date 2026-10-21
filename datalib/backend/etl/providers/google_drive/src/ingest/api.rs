//! The Google APIs this source reads, and how their answers are judged.
//! Every request is built here, so a test's playback fixture is keyed
//! the way the download asks. latchkey signs each by its host: Drive
//! with any of its four Google services, `docs.googleapis.com` with
//! `google-docs`, and so on. An image link the native JSON carries is
//! signed already and goes out plain.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use datalib_etl_google_drive_config::{ExportFormat, FileKind};
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpError, HttpRequest,
    HttpResponse, HttpService, LatchkeySettings, Retryability,
};

pub const HTTP_SERVICE: HttpService = HttpService::GoogleDrive;
pub const DRIVE: &str = "https://www.googleapis.com/drive/v3";
pub const DOCS: &str = "https://docs.googleapis.com/v1";
pub const SHEETS: &str = "https://sheets.googleapis.com/v4";
pub const SLIDES: &str = "https://slides.googleapis.com/v1";

/// A native Sheet runs to several MB, and an export can be 10.
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// Drive's page sizes are capped per endpoint.
const LIST_PAGE: u32 = 1000;
const REVISIONS_PAGE: u32 = 1000;
const COMMENTS_PAGE: u32 = 100;

/// What each kind is called on the wire, and who signs its native API.
pub struct KindInfo {
    pub kind: FileKind,
    pub mime_type: &'static str,
    pub latchkey_service: &'static str,
}

pub const KINDS: &[KindInfo] = &[
    KindInfo {
        kind: FileKind::Document,
        mime_type: "application/vnd.google-apps.document",
        latchkey_service: "google-docs",
    },
    KindInfo {
        kind: FileKind::Spreadsheet,
        mime_type: "application/vnd.google-apps.spreadsheet",
        latchkey_service: "google-sheets",
    },
    KindInfo {
        kind: FileKind::Presentation,
        mime_type: "application/vnd.google-apps.presentation",
        latchkey_service: "google-slides",
    },
];

pub fn kind_info(kind: FileKind) -> &'static KindInfo {
    KINDS
        .iter()
        .find(|k| k.kind == kind)
        .expect("KINDS names every FileKind")
}

pub fn kind_of_mime(mime_type: &str) -> Option<FileKind> {
    KINDS
        .iter()
        .find(|k| k.mime_type == mime_type)
        .map(|k| k.kind)
}

/// The MIME type Drive exports `kind` as in `format`, and the extension
/// the part is named with.
pub fn export_type(kind: FileKind, format: ExportFormat) -> (&'static str, &'static str) {
    use ExportFormat::*;
    use FileKind::*;
    match (kind, format) {
        (Document, Odf) => ("application/vnd.oasis.opendocument.text", "odt"),
        (Spreadsheet, Odf) => ("application/vnd.oasis.opendocument.spreadsheet", "ods"),
        (Presentation, Odf) => ("application/vnd.oasis.opendocument.presentation", "odp"),
        (Document, Ooxml) => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        (Spreadsheet, Ooxml) => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
        (Presentation, Ooxml) => (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
        (_, Pdf) => ("application/pdf", "pdf"),
    }
}

// ── URLs ─────────────────────────────────────────────────────────────

/// One page of the listing: every file of `kinds` not in the trash,
/// shared drives included, the account's own only when `owned_only`.
pub fn list_url(kinds: &[FileKind], owned_only: bool, page_token: Option<&str>) -> String {
    let types: Vec<String> = kinds
        .iter()
        .map(|k| format!("mimeType='{}'", kind_info(*k).mime_type))
        .collect();
    let mut q = format!("({}) and trashed=false", types.join(" or "));
    if owned_only {
        q.push_str(" and 'me' in owners");
    }
    let mut url = format!(
        "{DRIVE}/files?q={}&pageSize={LIST_PAGE}&fields={}&orderBy={}\
         &includeItemsFromAllDrives=true&supportsAllDrives=true",
        percent_encode(&q),
        percent_encode("nextPageToken,incompleteSearch,files(*)"),
        percent_encode("modifiedTime desc"),
    );
    push_page_token(&mut url, page_token);
    url
}

/// One named file's listing entry, for a config that names files.
pub fn file_url(file_id: &str) -> String {
    format!("{DRIVE}/files/{file_id}?fields=*&supportsAllDrives=true")
}

pub fn revisions_url(file_id: &str, page_token: Option<&str>) -> String {
    let mut url = format!(
        "{DRIVE}/files/{file_id}/revisions?pageSize={REVISIONS_PAGE}&fields={}",
        percent_encode("nextPageToken,revisions(*)"),
    );
    push_page_token(&mut url, page_token);
    url
}

pub fn comments_url(file_id: &str, page_token: Option<&str>) -> String {
    let mut url = format!(
        "{DRIVE}/files/{file_id}/comments?includeDeleted=true&pageSize={COMMENTS_PAGE}&fields={}",
        percent_encode("nextPageToken,comments(*)"),
    );
    push_page_token(&mut url, page_token);
    url
}

pub fn export_url(file_id: &str, mime_type: &str) -> String {
    format!(
        "{DRIVE}/files/{file_id}/export?mimeType={}",
        percent_encode(mime_type)
    )
}

/// The whole file in its native API's form: every tab of a Doc with its
/// suggestions inline, every cell of a Sheet, every page of a deck.
pub fn native_url(kind: FileKind, file_id: &str) -> String {
    match kind {
        FileKind::Document => format!(
            "{DOCS}/documents/{file_id}?includeTabsContent=true\
             &suggestionsViewMode=SUGGESTIONS_INLINE"
        ),
        FileKind::Spreadsheet => {
            format!("{SHEETS}/spreadsheets/{file_id}?includeGridData=true")
        }
        FileKind::Presentation => format!("{SLIDES}/presentations/{file_id}"),
    }
}

fn push_page_token(url: &mut String, page_token: Option<&str>) {
    if let Some(token) = page_token {
        url.push_str("&pageToken=");
        url.push_str(&percent_encode(token));
    }
}

// ── Requests ─────────────────────────────────────────────────────────

pub fn req_json(url: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url)
        .header("Accept", "application/json")
        .timeout(TIMEOUT)
}

pub fn req_bytes(url: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url).timeout(TIMEOUT)
}

/// An image link from the native JSON: signed already, so no latchkey.
pub fn req_image(url: &str) -> HttpRequest {
    req_bytes(url).plain()
}

/// Why a request came to nothing.
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    /// Nothing else on this service will fare better: latchkey would not
    /// send it (no sign-in), the credential was refused, a scope was not
    /// granted, or the API is off for the OAuth project.
    #[error("{0}")]
    Service(String),
    /// Google declined to export a file this large (10 MB).
    #[error("{0}")]
    ExportTooLarge(String),
    /// 404: no such file, or not one this account can see.
    #[error("{0}")]
    NotFound(String),
    /// About this one request: a file this account cannot read, a 5xx
    /// past its retries, a network failure.
    #[error("{0}")]
    Request(String),
}

pub fn is_service(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Service(_)))
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound(_)))
}

pub fn is_export_too_large(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ApiError>(),
        Some(ApiError::ExportTooLarge(_))
    )
}

pub async fn get_json(url: &str, lk: &LatchkeySettings) -> Result<Value> {
    let body = get(req_json(url).latchkey(lk.clone())).await?;
    serde_json::from_slice(&body).with_context(|| format!("parse JSON from {url}"))
}

/// The body as received, for bytes we keep verbatim.
pub async fn get_bytes(url: &str, lk: &LatchkeySettings) -> Result<Vec<u8>> {
    get(req_bytes(url).latchkey(lk.clone())).await
}

pub async fn get_image(url: &str) -> Result<Vec<u8>> {
    get(req_image(url)).await
}

async fn get(req: HttpRequest) -> Result<Vec<u8>> {
    let resp = latchkey_curl_classified(&req, retryability)
        .await
        .map_err(|e| transport_error(e, &req.url))?;
    if resp.status == 200 {
        return Ok(resp.body);
    }
    Err(status_error(&req.url, &resp).into())
}

/// Google answers a per-user rate limit with a 403 whose body says so;
/// that one is worth waiting out. Every other 403 is about access.
fn retryability(resp: &HttpResponse) -> Retryability {
    let body = resp.body_str();
    if resp.status == 403
        && (body.contains("rateLimitExceeded") || body.contains("userRateLimitExceeded"))
    {
        return Retryability::Retry { retry_after: None };
    }
    default_retryability(resp)
}

/// latchkey exits 1 when it will not send the request at all: no
/// credential for the service that signs this host. curl's own failures
/// (DNS, a refused connection, a timeout) have codes of their own.
fn transport_error(e: HttpError, url: &str) -> ApiError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            ApiError::Service(format!("GET {url}: {e}{}", sign_in_hint(url)))
        }
        other => ApiError::Request(other.to_string()),
    }
}

pub fn status_error(url: &str, resp: &HttpResponse) -> ApiError {
    let body = resp.body_str();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    let said = format!("GET {url}: {}: {message}", resp.status);
    let service_wide = resp.status == 401
        || (resp.status == 403
            && [
                "ACCESS_TOKEN_SCOPE_INSUFFICIENT",
                "accessNotConfigured",
                "SERVICE_DISABLED",
            ]
            .iter()
            .any(|reason| body.contains(reason)));
    if service_wide {
        ApiError::Service(format!("{said}{}", sign_in_hint(url)))
    } else if resp.status == 403 && body.contains("exportSizeLimitExceeded") {
        ApiError::ExportTooLarge(said)
    } else if resp.status == 404 {
        ApiError::NotFound(said)
    } else {
        ApiError::Request(said)
    }
}

/// Which latchkey service to sign in to again for `url`'s host.
fn sign_in_hint(url: &str) -> String {
    let service = KINDS
        .iter()
        .find(|k| url.starts_with(native_base(k.kind)))
        .map_or("google-drive", |k| k.latchkey_service);
    format!(
        ". Sign in with `latchkey auth browser {service}` and approve every scope it \
         asks for; if Google says the API is not enabled, run \
         `latchkey auth browser-prepare {service}` first"
    )
}

fn native_base(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Document => DOCS,
        FileKind::Spreadsheet => SHEETS,
        FileKind::Presentation => SLIDES,
    }
}

/// The JSON a page of a Drive list answered, with its items under `key`
/// and the token of the next page. A page without `key` is not a
/// listing at all.
pub fn page_items(v: &Value, key: &str) -> Result<(Vec<Value>, Option<String>)> {
    let items = v
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("the reply carried no `{key}` list"))?;
    let next = v
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok((items, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        }
    }

    #[test]
    fn a_request_latchkey_will_not_send_is_service_wide() {
        let e = transport_error(
            HttpError::Curl {
                service: HTTP_SERVICE,
                url: native_url(FileKind::Spreadsheet, "x"),
                exit: 1,
                stderr: "Error: No credentials found for google-sheets.".into(),
            },
            &native_url(FileKind::Spreadsheet, "x"),
        );
        assert!(matches!(e, ApiError::Service(_)), "{e}");
        assert!(e.to_string().contains("google-sheets"), "{e}");
    }

    #[test]
    fn a_network_failure_is_one_request() {
        let e = transport_error(
            HttpError::Curl {
                service: HTTP_SERVICE,
                url: "u".into(),
                exit: 7,
                stderr: String::new(),
            },
            "u",
        );
        assert!(matches!(e, ApiError::Request(_)), "{e}");
    }

    #[test]
    fn statuses_are_judged_by_what_they_say() {
        let url = native_url(FileKind::Document, "x");
        assert!(matches!(
            status_error(&url, &resp(401, "{}")),
            ApiError::Service(_)
        ));
        let scope = r#"{"error":{"code":403,"message":"Request had insufficient authentication scopes.","details":[{"reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#;
        let e = status_error(&url, &resp(403, scope));
        assert!(matches!(e, ApiError::Service(_)), "{e}");
        assert!(e.to_string().contains("google-docs"), "{e}");
        let big = r#"{"error":{"code":403,"message":"This file is too large to be exported.","errors":[{"reason":"exportSizeLimitExceeded"}]}}"#;
        assert!(matches!(
            status_error(&url, &resp(403, big)),
            ApiError::ExportTooLarge(_)
        ));
        let denied = r#"{"error":{"code":403,"message":"The caller does not have permission"}}"#;
        assert!(matches!(
            status_error(&url, &resp(403, denied)),
            ApiError::Request(_)
        ));
        assert!(matches!(
            status_error(&url, &resp(404, "{}")),
            ApiError::NotFound(_)
        ));
    }

    #[test]
    fn the_listing_asks_for_the_kinds_named() {
        let url = list_url(&[FileKind::Document], true, Some("tok"));
        assert!(
            url.contains("application%2Fvnd.google-apps.document"),
            "{url}"
        );
        assert!(!url.contains("spreadsheet"), "{url}");
        assert!(url.contains("%27me%27%20in%20owners"), "{url}");
        assert!(url.ends_with("&pageToken=tok"), "{url}");
    }

    #[test]
    fn every_kind_and_format_has_an_export_type() {
        for k in KINDS {
            for f in [ExportFormat::Odf, ExportFormat::Ooxml, ExportFormat::Pdf] {
                let (mime, ext) = export_type(k.kind, f);
                assert!(!mime.is_empty() && !ext.is_empty());
            }
            assert_eq!(kind_of_mime(k.mime_type), Some(k.kind));
        }
    }
}
