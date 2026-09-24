//! The WebDAV half of CardDAV and CalDAV: sending a PROPFIND or REPORT,
//! finding the account's principal, and walking the `multistatus` reply.
//! Servers disagree on namespace prefixes (`d:`, `D:`, none) and on text
//! encoding (Fastmail wraps every value in CDATA), so the walker matches
//! elements by local name and hands each provider the properties it asked
//! for through [`DavProps`].

use std::collections::BTreeMap;
use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use thiserror::Error;

use crate::http::{
    latchkey_curl, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpService, LatchkeySettings,
};

/// How many redirects one request follows. RFC 6764's `.well-known`
/// hop is the common one; a chain longer than this is a loop.
const MAX_REDIRECTS: usize = 5;

#[derive(Error, Debug)]
pub enum DavError {
    #[error("{service} transport: {source}")]
    Transport {
        service: HttpService,
        source: HttpError,
    },
    #[error("{service} http {status} on {method:?} {url}")]
    Http {
        service: HttpService,
        method: HttpMethod,
        status: u16,
        url: String,
    },
    #[error("{service} malformed response from {url}: {message}")]
    Malformed {
        service: HttpService,
        url: String,
        message: String,
    },
    #[error(
        "no {service} principal found — tried {tried}. Check the server URL and that \
         latchkey holds a login for this host."
    )]
    NoPrincipal { service: HttpService, tried: String },
}

impl DavError {
    pub fn status(&self) -> Option<u16> {
        match self {
            DavError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// The properties one provider reads out of a `<response>`. The walker
/// calls `leaf` as each element inside it closes, with its local name,
/// its parent's, and its text untrimmed; and `empty` for each
/// self-closing element. The response's own `href` and `status` are the
/// walker's, and never reach either.
pub trait DavProps: Default {
    fn leaf(&mut self, name: &str, parent: &str, text: String);
    fn empty(&mut self, _name: &str, _parent: &str, _element: &BytesStart<'_>) {}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavResponse<P> {
    pub href: String,
    /// A status directly under `<response>`: 404 for a resource a sync
    /// reports deleted, 507 for a sync the server cut short. `None` when
    /// the response carries its properties in `<propstat>`s, whose
    /// statuses are about those properties, not the resource.
    pub status: Option<u16>,
    pub props: P,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Multistatus<P> {
    pub responses: Vec<DavResponse<P>>,
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

/// RFC 6578 `sync-collection` asking for each resource's etag and
/// `data_prop`, which `ns_decl` declares the prefix of. An empty token
/// asks for everything; it is written as an empty element because some
/// servers read an empty string literally and answer with no changes.
pub fn body_sync_collection(prev_token: &str, ns_decl: &str, data_prop: &str) -> String {
    let token = if prev_token.is_empty() {
        "<sync-token/>".to_string()
    } else {
        format!("<sync-token>{}</sync-token>", escape_xml(prev_token))
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<sync-collection xmlns="DAV:" {ns_decl}>
  {token}
  <sync-level>1</sync-level>
  <prop>
    <getetag/>
    <{data_prop}/>
  </prop>
</sync-collection>
"#
    )
}

pub fn escape_xml(s: &str) -> String {
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

pub async fn propfind<P: DavProps>(
    service: HttpService,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus<P>, DavError> {
    request(service, HttpMethod::Propfind, url, depth, body, latchkey).await
}

pub async fn report<P: DavProps>(
    service: HttpService,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus<P>, DavError> {
    request(service, HttpMethod::Report, url, depth, body, latchkey).await
}

/// One DAV request, following redirects: the transport does not.
async fn request<P: DavProps>(
    service: HttpService,
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus<P>, DavError> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let req = http_request(service, method, &url, depth, body, latchkey);
        let resp = latchkey_curl(&req)
            .await
            .map_err(|source| DavError::Transport { service, source })?;
        if let Some(next) = redirect_target(&url, &resp) {
            url = next;
            continue;
        }
        if resp.status != 207 && resp.status != 200 {
            return Err(DavError::Http {
                service,
                method,
                status: resp.status,
                url,
            });
        }
        return parse_multistatus(&resp.body_str()).map_err(|e| DavError::Malformed {
            service,
            url,
            message: format!("xml: {e}"),
        });
    }
    Err(DavError::Malformed {
        service,
        url,
        message: format!("more than {MAX_REDIRECTS} redirects"),
    })
}

/// The request one DAV call sends — public so a test's playback fixture
/// is keyed exactly as the client asks.
pub fn http_request(
    service: HttpService,
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
        service,
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

#[derive(Default)]
struct PrincipalProps {
    principal: Option<String>,
}

impl DavProps for PrincipalProps {
    fn leaf(&mut self, name: &str, parent: &str, text: String) {
        let href = text.trim();
        if name == "href" && parent == "current-user-principal" && !href.is_empty() {
            self.principal = Some(href.to_string());
        }
    }
}

/// The account's principal URL: `current-user-principal` from the
/// configured URL, then from the host's `/.well-known/<well_known>`
/// (RFC 6764) when that one does not name it — Fastmail's bare hosts
/// answer 404. Each request sent is counted into `requests`.
pub async fn find_principal(
    service: HttpService,
    server_url: &str,
    well_known: &str,
    latchkey: &LatchkeySettings,
    requests: &mut usize,
) -> Result<String, DavError> {
    let mut candidates = vec![server_url.to_string()];
    if let Some(o) = origin(server_url) {
        candidates.push(format!("{o}/.well-known/{well_known}"));
    }
    let mut tried: Vec<String> = Vec::new();
    for url in candidates {
        *requests += 1;
        let found =
            propfind::<PrincipalProps>(service, &url, "0", BODY_CURRENT_USER_PRINCIPAL, latchkey)
                .await;
        match found {
            Ok(ms) => {
                let principal = ms
                    .responses
                    .into_iter()
                    .find_map(|r| r.props.principal)
                    .and_then(|href| absolutize(&url, &href));
                if let Some(principal) = principal {
                    return Ok(principal);
                }
                tried.push(format!("{url}: no current-user-principal"));
            }
            Err(e) => tried.push(format!("{url}: {e}")),
        }
    }
    Err(DavError::NoPrincipal {
        service,
        tried: tried.join("; "),
    })
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

pub fn parse_multistatus<P: DavProps>(body: &str) -> Result<Multistatus<P>, quick_xml::Error> {
    let mut reader = Reader::from_str(body);
    let mut out = Multistatus::default();
    let mut stack: Vec<String> = Vec::with_capacity(16);
    let mut current: Option<DavResponse<P>> = None;
    let mut text = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    current = Some(DavResponse::default());
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                if let Some(c) = current.as_mut() {
                    let parent = stack.last().map_or("", String::as_str);
                    c.props.empty(&local_name(e.name().as_ref()), parent, &e);
                }
            }
            Event::End(_) => {
                let Some(name) = stack.pop() else { continue };
                let parent = stack.last().map_or("", String::as_str);
                let value = std::mem::take(&mut text);
                if name == "response" {
                    out.responses.extend(current.take());
                } else if let Some(c) = current.as_mut() {
                    match (name.as_str(), parent) {
                        ("href", "response") if c.href.is_empty() => {
                            c.href = value.trim().to_string()
                        }
                        ("status", "response") => c.status = parse_status_code(&value),
                        _ => c.props.leaf(&name, parent, value),
                    }
                } else if name == "sync-token" {
                    let token = value.trim();
                    if !token.is_empty() {
                        out.sync_token = Some(token.to_string());
                    }
                }
            }
            Event::Eof => break,
            event => {
                if let Some(t) = crate::xml::text_of(&event, false) {
                    text.push_str(&t);
                }
            }
        }
    }
    Ok(out)
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

    #[derive(Debug, Default, PartialEq)]
    struct Seen {
        leaves: Vec<(String, String, String)>,
        empties: Vec<(String, String)>,
    }

    impl DavProps for Seen {
        fn leaf(&mut self, name: &str, parent: &str, text: String) {
            if !text.trim().is_empty() {
                self.leaves.push((name.into(), parent.into(), text));
            }
        }
        fn empty(&mut self, name: &str, parent: &str, _element: &BytesStart<'_>) {
            self.empties.push((name.into(), parent.into()));
        }
    }

    /// The walker keeps the response's own href and status, passes every
    /// other leaf through with its parent, and reads CDATA and references
    /// as text without trimming them — a vCard's CRLFs are its content.
    #[test]
    fn walks_prefixes_cdata_references_and_statuses() {
        let body = "<?xml version=\"1.0\"?>
<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:carddav\">
  <D:response>
    <D:href>/ab/riker.vcf</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/><C:addressbook/></D:resourcetype>
        <D:displayname><![CDATA[Personal]]></D:displayname>
        <C:address-data>BEGIN:VCARD&#13;\nUID:r&amp;1&#13;\nEND:VCARD&#13;\n</C:address-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
    <D:propstat><D:prop><D:getctag/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
  </D:response>
  <D:response><D:href>/ab/gone.vcf</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>
  <D:sync-token><![CDATA[data:,42]]></D:sync-token>
</D:multistatus>";
        let ms: Multistatus<Seen> = parse_multistatus(body).unwrap();
        assert_eq!(ms.sync_token.as_deref(), Some("data:,42"));
        let [riker, gone] = &ms.responses[..] else {
            panic!("two responses: {:?}", ms.responses)
        };
        assert_eq!(riker.href, "/ab/riker.vcf");
        assert_eq!(
            riker.status, None,
            "propstat statuses are not the response's"
        );
        let leaf = |name: &str| {
            riker
                .props
                .leaves
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, parent, text)| (parent.as_str(), text.as_str()))
        };
        assert_eq!(leaf("displayname"), Some(("prop", "Personal")));
        assert_eq!(
            leaf("address-data"),
            Some(("prop", "BEGIN:VCARD\r\nUID:r&1\r\nEND:VCARD\r\n"))
        );
        assert_eq!(leaf("href"), None, "the response href is the walker's");
        assert!(riker
            .props
            .empties
            .contains(&("addressbook".into(), "resourcetype".into())));
        assert_eq!(gone.href, "/ab/gone.vcf");
        assert_eq!(gone.status, Some(404));
    }

    #[test]
    fn a_principal_reply_names_its_principal() {
        let body = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/</d:href><d:propstat><d:prop>
<d:current-user-principal><d:href>/dav/principals/user/picard@enterprise.test/</d:href></d:current-user-principal>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let ms: Multistatus<PrincipalProps> = parse_multistatus(body).unwrap();
        assert_eq!(ms.responses[0].href, "/dav/");
        assert_eq!(
            ms.responses[0].props.principal.as_deref(),
            Some("/dav/principals/user/picard@enterprise.test/")
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
        let ns = r#"xmlns:C="urn:ietf:params:xml:ns:caldav""#;
        assert!(body_sync_collection("", ns, "C:calendar-data").contains("<sync-token/>"));
        let body = body_sync_collection("a&b", ns, "C:calendar-data");
        assert!(body.contains("<sync-token>a&amp;b</sync-token>"));
        assert!(body.contains("<C:calendar-data/>"));
    }
}
