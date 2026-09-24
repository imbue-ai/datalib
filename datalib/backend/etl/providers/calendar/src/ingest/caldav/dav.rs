//! The WebDAV half of CalDAV: the requests, and reading a `multistatus`
//! reply. Servers disagree on prefixes (`d:`, `D:`, none) and on text
//! encoding (Fastmail wraps every value in CDATA), so the reader walks
//! events and keeps the leaves it knows by local name.

use std::collections::BTreeMap;
use std::time::Duration;

use quick_xml::events::Event;
use quick_xml::Reader;
use thiserror::Error;

use datalib_etl::http::{
    latchkey_curl, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpService, LatchkeySettings,
};

pub const HTTP_SERVICE: HttpService = HttpService::Caldav;

/// How many redirects one request follows. RFC 6764's `.well-known`
/// hop is the common one; a chain longer than this is a loop.
const MAX_REDIRECTS: usize = 5;

#[derive(Error, Debug)]
pub enum DavError {
    #[error("caldav transport: {0}")]
    Transport(#[from] HttpError),
    #[error("caldav http {status} on {method:?} {url}")]
    Http {
        method: HttpMethod,
        status: u16,
        url: String,
    },
    #[error("caldav malformed response from {url}: {message}")]
    Malformed { url: String, message: String },
}

impl DavError {
    pub fn status(&self) -> Option<u16> {
        match self {
            DavError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// One `<response>` of a multistatus. A property is `Some` only when
/// the server returned it with a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavResponse {
    pub href: String,
    /// A status directly under `<response>`: 404 for a resource a sync
    /// reports deleted, 507 for a sync the server cut short. `None`
    /// when the response carries its properties in `<propstat>`s.
    pub status: Option<u16>,
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    /// `calendar-timezone`: a whole `VCALENDAR` holding one `VTIMEZONE`.
    pub calendar_timezone: Option<String>,
    pub current_user_principal: Option<String>,
    pub calendar_home_set: Option<String>,
    /// `calendar-user-address-set` entries, in order (`mailto:…`).
    pub user_addresses: Vec<String>,
    /// `<resourcetype>` holds `<calendar/>`. Scheduling inboxes and
    /// outboxes do not, and are not calendars to mirror.
    pub is_calendar: bool,
    /// The components `supported-calendar-component-set` names; empty
    /// when the server did not say, which means any.
    pub components: Vec<String>,
    pub calendar_data: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Multistatus {
    pub responses: Vec<DavResponse>,
    /// The root-level `<sync-token>` of a `sync-collection` reply.
    pub sync_token: Option<String>,
}

pub const BODY_CURRENT_USER_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop>
    <current-user-principal/>
  </prop>
</propfind>
"#;

pub const BODY_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop>
    <C:calendar-home-set/>
    <C:calendar-user-address-set/>
  </prop>
</propfind>
"#;

pub const BODY_LIST_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <prop>
    <resourcetype/>
    <displayname/>
    <C:calendar-description/>
    <C:calendar-timezone/>
    <C:supported-calendar-component-set/>
    <A:calendar-color/>
  </prop>
</propfind>
"#;

/// RFC 6578 `sync-collection`. An empty token asks for everything; it
/// is written as an empty element because some servers read an empty
/// string literally and answer with no changes.
pub fn body_sync_collection(prev_token: &str) -> String {
    let token = if prev_token.is_empty() {
        "<sync-token/>".to_string()
    } else {
        format!("<sync-token>{}</sync-token>", escape_xml(prev_token))
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<sync-collection xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  {token}
  <sync-level>1</sync-level>
  <prop>
    <getetag/>
    <C:calendar-data/>
  </prop>
</sync-collection>
"#
    )
}

/// RFC 4791 `calendar-query` for every event: the listing a server that
/// cannot `sync-collection` still answers.
pub const BODY_QUERY_ALL_EVENTS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop>
    <getetag/>
    <C:calendar-data/>
  </prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
"#;

/// `calendar-query` for one window: the events with an instance in it,
/// each series trimmed to the overrides that fall in it. RFC 4791 wants
/// both bounds on `limit-recurrence-set`, so an open end is spelled as
/// a far one.
pub fn body_query_window(window: &super::super::Window) -> String {
    let stamp = |d: chrono::NaiveDate| d.format("%Y%m%dT000000Z").to_string();
    let start = window
        .start
        .map(stamp)
        .unwrap_or_else(|| "19000101T000000Z".into());
    let end = window
        .end
        .map(stamp)
        .unwrap_or_else(|| "30000101T000000Z".into());
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop>
    <getetag/>
    <C:calendar-data>
      <C:limit-recurrence-set start="{start}" end="{end}"/>
    </C:calendar-data>
  </prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start}" end="{end}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
"#
    )
}

/// RFC 4791 `calendar-multiget`, for the resources a listing named
/// without their data.
pub fn body_multiget(hrefs: &[String]) -> String {
    let hrefs: String = hrefs
        .iter()
        .map(|h| format!("  <href>{}</href>\n", escape_xml(h)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-multiget xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop>
    <getetag/>
    <C:calendar-data/>
  </prop>
{hrefs}</C:calendar-multiget>
"#
    )
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

pub async fn propfind(
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    request(HttpMethod::Propfind, url, depth, body, latchkey).await
}

pub async fn report(
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    request(HttpMethod::Report, url, depth, body, latchkey).await
}

/// One DAV request, following redirects: the transport does not.
async fn request(
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let req = http_request(method, &url, depth, body, latchkey);
        let resp = latchkey_curl(&req).await?;
        if let Some(next) = redirect_target(&url, &resp) {
            url = next;
            continue;
        }
        if resp.status != 207 && resp.status != 200 {
            return Err(DavError::Http {
                method,
                status: resp.status,
                url,
            });
        }
        return parse_multistatus(&url, &resp.body_str());
    }
    Err(DavError::Malformed {
        url,
        message: format!("more than {MAX_REDIRECTS} redirects"),
    })
}

/// The request one DAV call sends — public so a test's playback
/// fixture is keyed exactly as the client asks.
pub fn http_request(
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> HttpRequest {
    let mut headers = BTreeMap::new();
    headers.insert("Depth".to_string(), depth.to_string());
    headers.insert(
        "Content-Type".to_string(),
        "application/xml; charset=utf-8".to_string(),
    );
    HttpRequest {
        service: HTTP_SERVICE,
        method,
        url: url.to_string(),
        headers,
        body: Some(body.as_bytes().to_vec()),
        timeout: Duration::from_secs(180),
        bypass_latchkey: false,
        latchkey: latchkey.clone(),
        bearer: None,
    }
}

fn redirect_target(url: &str, resp: &HttpResponse) -> Option<String> {
    if !matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    absolutize(url, resp.header("location")?)
}

/// `href` against `base`: servers hand back absolute URLs, root-relative
/// paths, and (rarely) paths relative to the request.
pub fn absolutize(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let origin = origin(base)?;
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    let dir = match base.rfind('/') {
        Some(i) if i + 1 > origin.len() => &base[..=i],
        _ => return Some(format!("{origin}/{href}")),
    };
    Some(format!("{dir}{href}"))
}

/// `scheme://host[:port]` of a URL.
pub fn origin(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")? + 3;
    let host_end = url[scheme_end..]
        .find('/')
        .map_or(url.len(), |i| scheme_end + i);
    (host_end > scheme_end).then(|| &url[..host_end])
}

pub fn parse_multistatus(url: &str, body: &str) -> Result<Multistatus, DavError> {
    let mut reader = Reader::from_str(body);
    let mut out = Multistatus::default();
    let mut stack: Vec<String> = Vec::with_capacity(16);
    let mut current: Option<DavResponse> = None;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    current = Some(DavResponse::default());
                }
                stack.push(name);
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                let parent = stack.last().map(String::as_str);
                if let Some(c) = current.as_mut() {
                    if name == "calendar" && parent == Some("resourcetype") {
                        c.is_calendar = true;
                    }
                    if name == "comp" && parent == Some("supported-calendar-component-set") {
                        // A component name is plain ASCII: nothing to unescape.
                        if let Ok(Some(attr)) = e.try_get_attribute("name") {
                            c.components
                                .push(String::from_utf8_lossy(&attr.value).to_ascii_uppercase());
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => text.push_str(&t.decode().unwrap_or_default()),
            Ok(Event::CData(t)) => text.push_str(&String::from_utf8_lossy(&t)),
            Ok(Event::GeneralRef(r)) => text.push_str(&datalib_etl::xml::reference_text(&r, false)),
            Ok(Event::End(_)) => {
                let Some(name) = stack.pop() else { continue };
                let parent = stack.last().map(String::as_str).unwrap_or("");
                let value = std::mem::take(&mut text);
                if name == "response" {
                    if let Some(r) = current.take() {
                        out.responses.push(r);
                    }
                } else if name == "sync-token" && current.is_none() {
                    let tok = value.trim();
                    if !tok.is_empty() {
                        out.sync_token = Some(tok.to_string());
                    }
                } else if let Some(c) = current.as_mut() {
                    apply_leaf(c, &name, parent, value);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(DavError::Malformed {
                    url: url.to_string(),
                    message: format!("xml: {e}"),
                })
            }
            _ => {}
        }
    }
    Ok(out)
}

fn apply_leaf(c: &mut DavResponse, leaf: &str, parent: &str, value: String) {
    let trimmed = value.trim();
    let some = || (!trimmed.is_empty()).then(|| trimmed.to_string());
    match (leaf, parent) {
        ("href", "response") if c.href.is_empty() => c.href = trimmed.to_string(),
        ("href", "current-user-principal") => c.current_user_principal = some(),
        ("href", "calendar-home-set") => c.calendar_home_set = some(),
        ("href", "calendar-user-address-set") => c.user_addresses.extend(some()),
        ("status", "response") => c.status = parse_status_code(trimmed),
        ("getetag", _) => c.etag = some(),
        ("displayname", _) => c.display_name = some(),
        ("calendar-description", _) => c.description = some(),
        ("calendar-color", _) => c.color = some(),
        ("calendar-timezone", _) => c.calendar_timezone = some(),
        // Not trimmed: the object is what the server stored.
        ("calendar-data", _) if !trimmed.is_empty() => c.calendar_data = Some(value),
        _ => {}
    }
}

fn local_name(name: &[u8]) -> String {
    let s = std::str::from_utf8(name).unwrap_or("");
    s.rsplit_once(':').map_or(s, |(_, local)| local).to_string()
}

/// `HTTP/1.1 404 Not Found` → 404.
fn parse_status_code(s: &str) -> Option<u16> {
    s.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fastmail's shape, verified against a live account: unprefixed
    /// `DAV:`, a `C:` prefix for CalDAV, every value in CDATA, a 404
    /// propstat beside the 200 one, and scheduling boxes in the listing.
    const LISTING: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <response>
    <href>/dav/calendars/user/picard@enterprise.test/</href>
    <propstat><prop><resourcetype><collection/></resourcetype><displayname><![CDATA[Jean-Luc Picard]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
  <response>
    <href>/dav/calendars/user/picard@enterprise.test/2c1f4e0a-bridge/</href>
    <propstat>
      <prop>
        <resourcetype><collection/><C:calendar/></resourcetype>
        <displayname><![CDATA[Bridge Duty]]></displayname>
        <C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set>
        <C:calendar-timezone><![CDATA[BEGIN:VCALENDAR
BEGIN:VTIMEZONE
TZID:America/Los_Angeles
END:VTIMEZONE
END:VCALENDAR
]]></C:calendar-timezone>
        <A:calendar-color><![CDATA[#16A765]]></A:calendar-color>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
    <propstat><prop><C:calendar-description/></prop><status>HTTP/1.1 404 Not Found</status></propstat>
  </response>
  <response>
    <href>/dav/calendars/user/picard@enterprise.test/Inbox/</href>
    <propstat><prop><resourcetype><collection/><C:schedule-inbox/></resourcetype><displayname><![CDATA[Inbox]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#;

    #[test]
    fn reads_cdata_and_tells_calendars_from_scheduling_boxes() {
        let ms = parse_multistatus("u", LISTING).unwrap();
        assert_eq!(ms.responses.len(), 3);
        let cals: Vec<&DavResponse> = ms.responses.iter().filter(|r| r.is_calendar).collect();
        assert_eq!(cals.len(), 1);
        let bridge = cals[0];
        assert_eq!(bridge.display_name.as_deref(), Some("Bridge Duty"));
        assert_eq!(bridge.color.as_deref(), Some("#16A765"));
        assert_eq!(bridge.components, vec!["VEVENT"]);
        assert!(bridge
            .calendar_timezone
            .as_deref()
            .unwrap()
            .contains("TZID:America/Los_Angeles"));
        assert_eq!(
            bridge.status, None,
            "propstat statuses are not the response's"
        );
        assert_eq!(bridge.description, None);
    }

    #[test]
    fn a_sync_reply_names_changes_deletions_and_its_token() {
        let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/bridge/staff.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>BEGIN:VCALENDAR&#13;
BEGIN:VEVENT&#13;
UID:staff&#13;
END:VEVENT&#13;
END:VCALENDAR&#13;
</cal:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response><d:href>/cal/bridge/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>
  <d:sync-token>data:,42</d:sync-token>
</d:multistatus>"#;
        let ms = parse_multistatus("u", body).unwrap();
        assert_eq!(ms.sync_token.as_deref(), Some("data:,42"));
        assert_eq!(ms.responses[0].etag.as_deref(), Some("\"e1\""));
        assert!(ms.responses[0]
            .calendar_data
            .as_deref()
            .unwrap()
            .contains("UID:staff\r\n"));
        assert_eq!(ms.responses[1].status, Some(404));
    }

    #[test]
    fn principal_reply_yields_home_and_addresses() {
        let body = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/p/</href><propstat><prop>
<C:calendar-home-set><href>/dav/calendars/user/picard@enterprise.test/</href></C:calendar-home-set>
<C:calendar-user-address-set><href>mailto:picard@enterprise.test</href><href>/p/</href></C:calendar-user-address-set>
</prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        let ms = parse_multistatus("u", body).unwrap();
        let r = &ms.responses[0];
        assert_eq!(r.href, "/p/");
        assert_eq!(
            r.calendar_home_set.as_deref(),
            Some("/dav/calendars/user/picard@enterprise.test/")
        );
        assert_eq!(
            r.user_addresses,
            vec!["mailto:picard@enterprise.test", "/p/"]
        );
    }

    #[test]
    fn absolutize_handles_every_href_form() {
        assert_eq!(
            absolutize(
                "https://caldav.fastmail.com/dav/",
                "/dav/principals/user/x/"
            )
            .as_deref(),
            Some("https://caldav.fastmail.com/dav/principals/user/x/")
        );
        assert_eq!(
            absolutize("https://a.test/dav/", "https://p1.a.test/x/").as_deref(),
            Some("https://p1.a.test/x/")
        );
        assert_eq!(
            absolutize("https://a.test/dav/cal/", "e.ics").as_deref(),
            Some("https://a.test/dav/cal/e.ics")
        );
        assert_eq!(
            absolutize("https://a.test", "e.ics").as_deref(),
            Some("https://a.test/e.ics")
        );
        assert_eq!(origin("https://a.test:8443/x"), Some("https://a.test:8443"));
    }

    #[test]
    fn a_first_sync_sends_an_empty_token_element() {
        assert!(body_sync_collection("").contains("<sync-token/>"));
        assert!(body_sync_collection("a&b").contains("<sync-token>a&amp;b</sync-token>"));
    }
}
