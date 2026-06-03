//! CardDAV client built on top of [`frankweiler_etl::http`].
//!
//! Discovery walks `current-user-principal` → principal URL →
//! `addressbook-home-set` → addressbook list (each step one
//! PROPFIND). Incremental sync is one `sync-collection` REPORT per
//! addressbook with the persisted sync-token; the response carries
//! per-href etags, the vCard `<address-data>` payloads inline, and
//! a new sync-token at the document root. Servers that don't honor
//! sync-collection fall back to a ctag check + etag walk.
//!
//! Auth headers are injected by latchkey based on URL host (see
//! `frankweiler_etl::http`). Callers do NOT touch credentials here.
//!
//! ## XML parsing strategy
//!
//! Multistatus responses come back with variable namespace prefixes
//! across server implementations (Apple emits `d:` / `card:`,
//! Fastmail `D:` / `C:`, Google all-`d:`-with-card-namespace, etc).
//! Rather than wire up a full namespace-aware deserializer, we walk
//! the event stream from `quick-xml` and match on **local element
//! names** — DAV-defined names like `response`, `href`, `propstat`
//! and CardDAV-defined names like `address-data`. Both URI
//! namespaces (`DAV:` and `urn:ietf:params:xml:ns:carddav`) only
//! contain elements with disjoint local names, so local-name match
//! is unambiguous in practice and forgives every prefix quirk
//! we've encountered.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;
use thiserror::Error;

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpMethod, HttpRequest, HttpResponse};

/// Latchkey provider tag for every CardDAV request. The trailing
/// host-specific keying happens inside latchkey based on the URL
/// host; this value is just what shows up in playback fixtures +
/// telemetry events.
pub const PROVIDER: &str = "carddav";

#[derive(Error, Debug)]
pub enum CarddavError {
    #[error("carddav transport: {0}")]
    Transport(#[from] HttpError),
    /// Server responded with a non-success HTTP status that we don't
    /// know how to retry/recover.
    #[error("carddav http {status} on {method:?} {url}")]
    Http {
        method: HttpMethod,
        status: u16,
        url: String,
    },
    /// Multistatus response we couldn't parse — usually a server
    /// bug or unexpected namespace.
    #[error("carddav malformed response from {url}: {message}")]
    Malformed { url: String, message: String },
}

/// One `<response>` block out of a multistatus document. `href` is
/// the per-resource URL the server keyed this response under; the
/// other fields are filled in iff the corresponding sub-elements
/// appeared (CardDAV servers vary widely in which properties they
/// return, so every column is optional).
#[derive(Debug, Clone, Default)]
pub struct DavResponse {
    pub href: String,
    /// `<status>HTTP/1.1 <code> ...</status>` from the first
    /// propstat block — defaults to 200 when absent.
    pub status: u16,
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    /// `<getctag>` (the AppleCalServer "cheap collection version"
    /// extension, since standardized as `<sync-token>` but
    /// universally implemented under the older name too).
    pub ctag: Option<String>,
    pub current_user_principal: Option<String>,
    pub addressbook_home_set: Option<String>,
    /// Resource type hints — true when `<resourcetype>` contains
    /// `<addressbook/>`. Used to filter the home-set listing down
    /// to actual addressbooks (vs. proxies, calendars, etc).
    pub is_addressbook: bool,
    /// The raw vCard bytes from `<address-data>`. Only present on
    /// sync-collection / multiget responses, not on PROPFINDs.
    pub vcard: Option<String>,
}

/// Parsed multistatus document.
#[derive(Debug, Clone, Default)]
pub struct Multistatus {
    pub responses: Vec<DavResponse>,
    /// Root-level `<sync-token>` returned on a sync-collection
    /// REPORT. The next REPORT carries this back to the server.
    pub sync_token: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────
// Request bodies
// ─────────────────────────────────────────────────────────────────────

/// PROPFIND body that asks for `current-user-principal` — the entry
/// point of every discovery flow. Depth `0`.
pub const BODY_CURRENT_USER_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop>
    <current-user-principal/>
  </prop>
</propfind>
"#;

/// PROPFIND body asking for `addressbook-home-set` on a principal
/// URL. Depth `0`.
pub const BODY_ADDRESSBOOK_HOME_SET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <prop>
    <card:addressbook-home-set/>
  </prop>
</propfind>
"#;

/// PROPFIND body listing addressbooks under a home-set URL. Asks for
/// the metadata we promote to columns. Depth `1`.
pub const BODY_LIST_ADDRESSBOOKS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">
  <prop>
    <resourcetype/>
    <displayname/>
    <card:addressbook-description/>
    <cs:getctag/>
  </prop>
</propfind>
"#;

/// `sync-collection` REPORT body. The caller substitutes
/// `{SYNC_TOKEN}` with either the previously-stored token (for an
/// incremental sync) or an empty string (for a full enumeration).
pub fn body_sync_collection(prev_token: &str) -> String {
    // Some servers (notably Apple) interpret the empty-string token
    // literally and return zero changes. The canonical way to say
    // "give me everything" is an empty `<sync-token/>` element.
    let token_xml = if prev_token.is_empty() {
        "<sync-token/>".to_string()
    } else {
        format!("<sync-token>{}</sync-token>", escape_xml(prev_token))
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<sync-collection xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  {token_xml}
  <sync-level>1</sync-level>
  <prop>
    <getetag/>
    <card:address-data/>
  </prop>
</sync-collection>
"#
    )
}

/// `addressbook-multiget` REPORT body listing hrefs to fetch in one
/// shot. Used as the etag-walk fallback when a server doesn't
/// honor `sync-collection`.
pub fn body_addressbook_multiget(hrefs: &[String]) -> String {
    let mut body = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?>
<card:addressbook-multiget xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <prop>
    <getetag/>
    <card:address-data/>
  </prop>
"#,
    );
    for h in hrefs {
        body.push_str("  <href>");
        body.push_str(&escape_xml(h));
        body.push_str("</href>\n");
    }
    body.push_str("</card:addressbook-multiget>\n");
    body
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// Request helpers
// ─────────────────────────────────────────────────────────────────────

/// Issue a PROPFIND with the given XML body and depth header. Caller
/// supplies the parsed multistatus.
pub async fn propfind(url: &str, depth: &str, body: &str) -> Result<Multistatus, CarddavError> {
    let req = HttpRequest {
        provider: PROVIDER,
        method: HttpMethod::Propfind,
        url: url.to_string(),
        headers: {
            let mut h = std::collections::BTreeMap::new();
            h.insert("Depth".into(), depth.into());
            h.insert("Content-Type".into(), "application/xml; charset=utf-8".into());
            h
        },
        body: Some(body.as_bytes().to_vec()),
        timeout: std::time::Duration::from_secs(60),
    };
    let resp = latchkey_curl(&req).await?;
    expect_dav_status(&req.method, &req.url, &resp)?;
    parse_multistatus(&req.url, &resp.body_str())
}

/// Issue a sync-collection REPORT. Depth defaults to `0` per
/// RFC 6578 §3.
pub async fn report(url: &str, body: &str) -> Result<Multistatus, CarddavError> {
    let req = HttpRequest {
        provider: PROVIDER,
        method: HttpMethod::Report,
        url: url.to_string(),
        headers: {
            let mut h = std::collections::BTreeMap::new();
            h.insert("Depth".into(), "0".into());
            h.insert("Content-Type".into(), "application/xml; charset=utf-8".into());
            h
        },
        body: Some(body.as_bytes().to_vec()),
        timeout: std::time::Duration::from_secs(120),
    };
    let resp = latchkey_curl(&req).await?;
    expect_dav_status(&req.method, &req.url, &resp)?;
    parse_multistatus(&req.url, &resp.body_str())
}

/// 207 Multi-Status is the success status for both PROPFIND and
/// REPORT. Some servers also return 200 for PROPFIND when the
/// response is single-resource; tolerate that too.
fn expect_dav_status(
    method: &HttpMethod,
    url: &str,
    resp: &HttpResponse,
) -> Result<(), CarddavError> {
    if resp.status == 207 || resp.status == 200 {
        return Ok(());
    }
    Err(CarddavError::Http {
        method: *method,
        status: resp.status,
        url: url.to_string(),
    })
}

// ─────────────────────────────────────────────────────────────────────
// Multistatus parsing
// ─────────────────────────────────────────────────────────────────────

/// Walks the event stream and assembles a [`Multistatus`].
///
/// Strategy: track a small stack of element local names so we know
/// the path (e.g. `multistatus/response/propstat/prop/getetag`) when
/// text appears. We only key off local name; namespace prefixes
/// vary across servers and the URI-vs-URI ambiguity is moot because
/// the DAV and CardDAV vocabularies have disjoint locals.
pub fn parse_multistatus(url: &str, body: &str) -> Result<Multistatus, CarddavError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut out = Multistatus::default();
    let mut stack: Vec<String> = Vec::with_capacity(16);
    let mut current: Option<DavResponse> = None;
    let mut status_buf = String::new();
    let mut text_capture = TextCapture::default();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                stack.push(name.clone());
                if name == "response" {
                    current = Some(DavResponse {
                        // Default to 200 — many servers omit
                        // `<status>` when everything's fine and the
                        // resource itself doesn't carry a propstat
                        // (sync-collection inline `<address-data>`
                        // is one case).
                        status: 200,
                        ..Default::default()
                    });
                    status_buf.clear();
                }
                if name == "resourcetype" {
                    if let Some(c) = current.as_mut() {
                        c.is_addressbook = false;
                    }
                }
                text_capture.start(&name, &stack);
            }
            Ok(Event::Empty(e)) => {
                // Self-closing element. We care about `<addressbook/>`
                // inside `<resourcetype>` as the marker that a
                // collection is an addressbook.
                let name = local_name(e.name().as_ref());
                if name == "addressbook" {
                    if let Some(c) = current.as_mut() {
                        if stack.last().map(String::as_str) == Some("resourcetype") {
                            c.is_addressbook = true;
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let txt = t.unescape().unwrap_or_default().into_owned();
                text_capture.append(&txt);
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if let Some(text) = text_capture.finish(&name) {
                    apply_text(&mut current, &stack, &name, &text, &mut status_buf);
                }
                stack.pop();
                if name == "response" {
                    if let Some(mut r) = current.take() {
                        if !status_buf.is_empty() {
                            r.status = parse_status_code(&status_buf).unwrap_or(r.status);
                        }
                        out.responses.push(r);
                    }
                }
                if name == "sync-token" && current.is_none() {
                    // Root-level sync-token (not inside a response).
                    if let Some(tok) = text_capture.last_finished() {
                        if !tok.is_empty() {
                            out.sync_token = Some(tok);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(CarddavError::Malformed {
                    url: url.to_string(),
                    message: format!("xml: {e}"),
                });
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Element name with the namespace prefix stripped.
fn local_name(name: &[u8]) -> String {
    let s = std::str::from_utf8(name).unwrap_or("");
    match s.rsplit_once(':') {
        Some((_prefix, local)) => local.to_string(),
        None => s.to_string(),
    }
}

/// Parse the status code out of a line like
/// `HTTP/1.1 404 Not Found`. Tolerates leading whitespace +
/// missing status text.
fn parse_status_code(s: &str) -> Option<u16> {
    let trimmed = s.trim();
    let mut parts = trimmed.split_whitespace();
    let _proto = parts.next()?;
    let code = parts.next()?;
    code.parse().ok()
}

/// Routes captured text to the right field on the current
/// [`DavResponse`] based on the stack path. Splits out for testability.
fn apply_text(
    current: &mut Option<DavResponse>,
    stack: &[String],
    leaf: &str,
    text: &str,
    status_buf: &mut String,
) {
    // Root-level sync-token is handled at the End event in
    // `parse_multistatus` because it's outside any `<response>`.
    let Some(c) = current.as_mut() else { return };
    match leaf {
        "href" => {
            // Only the direct `response/href` counts as the response
            // URL — nested hrefs inside <current-user-principal> /
            // <addressbook-home-set> are the property values.
            if parent_is(stack, "response") {
                if c.href.is_empty() {
                    c.href = text.to_string();
                }
            } else if parent_is(stack, "current-user-principal") {
                c.current_user_principal = Some(text.to_string());
            } else if parent_is(stack, "addressbook-home-set") {
                c.addressbook_home_set = Some(text.to_string());
            }
        }
        "getetag" => c.etag = Some(text.to_string()),
        "displayname" => c.display_name = Some(text.to_string()),
        "addressbook-description" => c.description = Some(text.to_string()),
        "getctag" => c.ctag = Some(text.to_string()),
        "address-data" => c.vcard = Some(text.to_string()),
        "status" => {
            // The status applies to the propstat block we're inside;
            // we only track one per response (first one wins).
            if status_buf.is_empty() {
                *status_buf = text.to_string();
            }
        }
        _ => {}
    }
}

/// True when the immediate parent element in the stack (the one
/// above the current leaf) has the given local name. The stack
/// includes the leaf, so the parent is at `len - 2`.
fn parent_is(stack: &[String], parent: &str) -> bool {
    if stack.len() < 2 {
        return false;
    }
    stack[stack.len() - 2] == parent
}

/// Tiny helper for accumulating text content per element. We need
/// it because quick-xml emits text in chunks (whitespace, entities)
/// and the leaf element name is the same one we'll see on End.
#[derive(Default)]
struct TextCapture {
    /// Stack of (element_name, captured_text) pairs, mirroring the
    /// element stack so nested elements don't clobber each other.
    pending: Vec<(String, String)>,
    last: Option<String>,
}

impl TextCapture {
    fn start(&mut self, name: &str, _stack: &[String]) {
        self.pending.push((name.to_string(), String::new()));
    }

    fn append(&mut self, text: &str) {
        if let Some((_, buf)) = self.pending.last_mut() {
            buf.push_str(text);
        }
    }

    /// Pop the matching open element + return its accumulated text.
    /// `None` when there's no matching open frame (mis-nested input).
    fn finish(&mut self, name: &str) -> Option<String> {
        if self.pending.last().map(|(n, _)| n.as_str()) == Some(name) {
            let (_, text) = self.pending.pop().unwrap();
            self.last = Some(text.clone());
            return Some(text);
        }
        None
    }

    fn last_finished(&self) -> Option<String> {
        self.last.clone()
    }
}

// ─────────────────────────────────────────────────────────────────────
// vCard utility helpers
// ─────────────────────────────────────────────────────────────────────

/// Pull the `UID` line out of a vCard. RFC 6350 §6.7.6 mandates it,
/// but we tolerate its absence and return `None` so the caller can
/// synthesize a stable id from `(addressbook_id, href)` instead.
///
/// Handles RFC 6350 §3.2 line folding (continuation lines start with
/// a space or tab) by reconstructing logical lines before scanning.
pub fn vcard_uid(vcard: &str) -> Option<String> {
    extract_property(vcard, "UID")
}

/// Pull the `FN:` (formatted name) line out of a vCard. The
/// `display_name` promoted column reads this.
pub fn vcard_fn(vcard: &str) -> Option<String> {
    extract_property(vcard, "FN")
}

/// Pull the `REV:` (revision timestamp) line — useful for "last
/// modified upstream" sorts even when the server's etag is opaque.
pub fn vcard_rev(vcard: &str) -> Option<String> {
    extract_property(vcard, "REV")
}

fn extract_property(vcard: &str, name: &str) -> Option<String> {
    let unfolded = unfold_vcard_lines(vcard);
    for line in unfolded.lines() {
        // Property lines are `NAME[;params]:value`. Match the prefix
        // before any `;` or `:`.
        let head_end = line
            .find(|c: char| c == ':' || c == ';')
            .unwrap_or(line.len());
        let prop = &line[..head_end];
        if prop.eq_ignore_ascii_case(name) {
            if let Some(colon) = line.find(':') {
                let value = line[colon + 1..].trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn unfold_vcard_lines(vcard: &str) -> String {
    let mut out = String::with_capacity(vcard.len());
    for line in vcard.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            out.push_str(&line[1..]);
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

/// (`href` → (etag, vcard)) extracted from a multistatus the way
/// sync-collection / multiget returns it. Skips responses whose
/// propstat status indicates a delete (404 / 410) — those land in
/// [`deleted_hrefs`] instead.
pub fn changed_contacts(ms: &Multistatus) -> HashMap<String, (Option<String>, String)> {
    let mut out = HashMap::new();
    for r in &ms.responses {
        if matches!(r.status, 404 | 410) {
            continue;
        }
        if let Some(v) = &r.vcard {
            out.insert(r.href.clone(), (r.etag.clone(), v.clone()));
        }
    }
    out
}

/// hrefs the server reported as gone (404 / 410) on a
/// sync-collection response. The caller drops them from the local
/// store via [`super::db::RawDb::delete_contact`].
pub fn deleted_hrefs(ms: &Multistatus) -> Vec<String> {
    ms.responses
        .iter()
        .filter(|r| matches!(r.status, 404 | 410))
        .map(|r| r.href.clone())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Fastmail-shaped current-user-principal response (lowercase
    /// `d:` prefix). Verifies our parser is namespace-prefix
    /// tolerant.
    const PRINCIPAL_FASTMAIL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal>
          <d:href>/dav/principals/user/u%40example.com/</d:href>
        </d:current-user-principal>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    /// Apple-shaped principal response (uppercase `D:` prefix).
    const HOME_SET_APPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav">
  <D:response>
    <D:href>/123/principal/</D:href>
    <D:propstat>
      <D:prop>
        <C:addressbook-home-set>
          <D:href>/123/carddavhome/</D:href>
        </C:addressbook-home-set>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    const ADDRESSBOOK_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/dav/addressbooks/user/u%40example.com/Default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
        <d:displayname>Default</d:displayname>
        <cs:getctag>abc-1</cs:getctag>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/addressbooks/user/u%40example.com/Calendar/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>Calendar</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const SYNC_COLLECTION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/dav/addressbooks/user/u%40example.com/Default/abc.vcf</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"v1"</d:getetag>
        <card:address-data>BEGIN:VCARD&#13;
VERSION:3.0&#13;
UID:abc&#13;
FN:Alice&#13;
END:VCARD&#13;
</card:address-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/addressbooks/user/u%40example.com/Default/gone.vcf</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>http://example.com/sync/4242</d:sync-token>
</d:multistatus>"#;

    #[test]
    fn parses_current_user_principal_lowercase_prefix() {
        let ms = parse_multistatus("url", PRINCIPAL_FASTMAIL).unwrap();
        assert_eq!(ms.responses.len(), 1);
        let r = &ms.responses[0];
        assert_eq!(r.href, "/");
        assert_eq!(
            r.current_user_principal.as_deref(),
            Some("/dav/principals/user/u%40example.com/")
        );
    }

    #[test]
    fn parses_addressbook_home_set_uppercase_prefix() {
        let ms = parse_multistatus("url", HOME_SET_APPLE).unwrap();
        assert_eq!(ms.responses.len(), 1);
        assert_eq!(
            ms.responses[0].addressbook_home_set.as_deref(),
            Some("/123/carddavhome/")
        );
    }

    #[test]
    fn marks_resourcetype_addressbook_only_on_addressbook_collections() {
        let ms = parse_multistatus("url", ADDRESSBOOK_LIST).unwrap();
        assert_eq!(ms.responses.len(), 2);
        let abs: Vec<_> = ms
            .responses
            .iter()
            .filter(|r| r.is_addressbook)
            .map(|r| r.href.as_str())
            .collect();
        assert_eq!(
            abs,
            vec!["/dav/addressbooks/user/u%40example.com/Default/"]
        );
        // ctag still captured for the addressbook.
        let addr = ms
            .responses
            .iter()
            .find(|r| r.is_addressbook)
            .unwrap();
        assert_eq!(addr.ctag.as_deref(), Some("abc-1"));
        assert_eq!(addr.display_name.as_deref(), Some("Default"));
    }

    #[test]
    fn sync_collection_yields_changes_and_deletes_and_token() {
        let ms = parse_multistatus("url", SYNC_COLLECTION).unwrap();
        assert_eq!(ms.sync_token.as_deref(), Some("http://example.com/sync/4242"));
        let changed = changed_contacts(&ms);
        assert_eq!(changed.len(), 1);
        let (etag, vcard) = changed
            .get("/dav/addressbooks/user/u%40example.com/Default/abc.vcf")
            .unwrap();
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        assert!(vcard.contains("UID:abc"));
        let deleted = deleted_hrefs(&ms);
        assert_eq!(
            deleted,
            vec!["/dav/addressbooks/user/u%40example.com/Default/gone.vcf".to_string()]
        );
    }

    #[test]
    fn vcard_uid_extracts_with_and_without_params() {
        let v = "BEGIN:VCARD\nVERSION:4.0\nUID:abc-123\nFN:Pat\nEND:VCARD\n";
        assert_eq!(vcard_uid(v).as_deref(), Some("abc-123"));
        let v2 = "BEGIN:VCARD\nVERSION:4.0\nUID;VALUE=text:urn:uuid:abc-123\nEND:VCARD\n";
        assert_eq!(vcard_uid(v2).as_deref(), Some("urn:uuid:abc-123"));
    }

    #[test]
    fn vcard_uid_handles_line_folding() {
        // RFC 6350 §3.2: continuation lines start with a single
        // space; the leading space is dropped on join.
        let v = "BEGIN:VCARD\nUID:abc-12\n 3\nEND:VCARD\n";
        assert_eq!(vcard_uid(v).as_deref(), Some("abc-123"));
    }

    #[test]
    fn body_sync_collection_emits_empty_token_element_for_first_run() {
        let body = body_sync_collection("");
        assert!(body.contains("<sync-token/>"));
        let body = body_sync_collection("http://example.com/sync/42");
        assert!(body.contains("<sync-token>http://example.com/sync/42</sync-token>"));
    }

    #[test]
    fn escape_xml_handles_special_chars() {
        assert_eq!(
            escape_xml("Tom & <Jerry>"),
            "Tom &amp; &lt;Jerry&gt;"
        );
    }
}
