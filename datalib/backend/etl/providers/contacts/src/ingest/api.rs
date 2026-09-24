//! What CardDAV asks a WebDAV server and reads out of its replies (the
//! requests and the `multistatus` walk are [`datalib_etl::dav`]'s), and
//! the vCard helpers the ingest and render sides share.

use std::collections::HashMap;

use quick_xml::events::BytesStart;

use datalib_etl::dav::{self as webdav, DavProps};
use datalib_etl::http::{HttpService, LatchkeySettings};

pub use datalib_etl::dav::DavError;

/// The latchkey service every CardDAV request runs under. The trailing
/// host-specific keying happens inside latchkey based on the URL
/// host; this value is just what shows up in playback fixtures +
/// telemetry events.
pub const HTTP_SERVICE: HttpService = HttpService::Carddav;

pub type DavResponse = webdav::DavResponse<ContactProps>;
pub type Multistatus = webdav::Multistatus<ContactProps>;

/// The CardDAV properties of one `<response>`. Servers vary widely in
/// which they return, so each is `Some` only when it came back with a
/// value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactProps {
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    /// `<getctag>` (the AppleCalServer "cheap collection version"
    /// extension, since standardized as `<sync-token>` but
    /// universally implemented under the older name too).
    pub ctag: Option<String>,
    pub addressbook_home_set: Option<String>,
    /// `<resourcetype>` holds `<addressbook/>`: the home-set listing
    /// also names the home itself, and some servers proxies and
    /// calendars.
    pub is_addressbook: bool,
    /// The raw vCard from `<address-data>`. Only present on
    /// sync-collection / multiget responses, not on PROPFINDs.
    pub vcard: Option<String>,
}

impl DavProps for ContactProps {
    fn leaf(&mut self, name: &str, parent: &str, text: String) {
        let trimmed = text.trim();
        let some = || (!trimmed.is_empty()).then(|| trimmed.to_string());
        match (name, parent) {
            ("href", "addressbook-home-set") => self.addressbook_home_set = some(),
            ("getetag", _) => self.etag = some(),
            ("displayname", _) => self.display_name = some(),
            ("addressbook-description", _) => self.description = some(),
            ("getctag", _) => self.ctag = some(),
            // Not trimmed: the card is what the server stored.
            ("address-data", _) if !trimmed.is_empty() => self.vcard = Some(text),
            _ => {}
        }
    }

    fn empty(&mut self, name: &str, parent: &str, _element: &BytesStart<'_>) {
        if name == "addressbook" && parent == "resourcetype" {
            self.is_addressbook = true;
        }
    }
}

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

pub fn body_sync_collection(prev_token: &str) -> String {
    webdav::body_sync_collection(
        prev_token,
        r#"xmlns:card="urn:ietf:params:xml:ns:carddav""#,
        "card:address-data",
    )
}

pub async fn propfind(
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    webdav::propfind(HTTP_SERVICE, url, depth, body, latchkey).await
}

/// A REPORT at Depth `0`, as RFC 6578 has `sync-collection` sent.
pub async fn report(
    url: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    webdav::report(HTTP_SERVICE, url, "0", body, latchkey).await
}

// vCard utility helpers

/// Pull the `UID` line out of a vCard. RFC 6350 §6.7.6 mandates it,
/// but we tolerate its absence and return `None` so the caller can
/// synthesize a stable id from `(addressbook_id, href)` instead.
pub fn vcard_uid(vcard: &str) -> Option<String> {
    extract_property(vcard, "UID")
}

pub fn vcard_fn(vcard: &str) -> Option<String> {
    extract_property(vcard, "FN")
}

pub fn vcard_rev(vcard: &str) -> Option<String> {
    extract_property(vcard, "REV")
}

/// Pull the structured `N:` (name) line as `(family, given)`. RFC 6350
/// §6.2.2 orders the semicolon-separated components
/// `Family;Given;Additional;Prefixes;Suffixes`; we keep the first two
/// — "last name" and "first name". Either may be empty; returns `None`
/// only when the vCard has no `N` line at all. Used to synthesize a
/// stable id for UID-less exports (see `schema_raw::synthesized_name_uid`).
pub fn vcard_n_family_given(vcard: &str) -> Option<(String, String)> {
    let n = extract_property(vcard, "N")?;
    let mut parts = n.split(';');
    let family = parts.next().unwrap_or("").trim().to_string();
    let given = parts.next().unwrap_or("").trim().to_string();
    Some((family, given))
}

/// All occurrences of a vCard property, in document order. vCards
/// can repeat properties (multiple emails, phones, addresses) and
/// render cares about each one individually.
pub fn vcard_all(vcard: &str, name: &str) -> Vec<VcardProp> {
    let unfolded = unfold_vcard_lines(vcard);
    let mut out = Vec::new();
    for line in unfolded.lines() {
        if !property_name(line).eq_ignore_ascii_case(name) {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let head = &line[..colon];
        let value = line[colon + 1..].trim().to_string();
        if value.is_empty() {
            continue;
        }
        // Parse the parameter block between `;` separators after the
        // property name. We only surface a few keys callers care
        // about; everything else is left in `raw_params`.
        let mut params: Vec<(String, String)> = Vec::new();
        for chunk in head.split(';').skip(1) {
            if let Some((k, v)) = chunk.split_once('=') {
                params.push((k.trim().to_string(), v.trim().to_string()));
            } else if !chunk.is_empty() {
                params.push(("TYPE".into(), chunk.trim().to_string()));
            }
        }
        out.push(VcardProp { value, params });
    }
    out
}

/// One occurrence of a vCard property, with its parameters preserved
/// so render can render `EMAIL;TYPE=work:p@…` as "work email"
/// rather than just "email".
#[derive(Debug, Clone)]
pub struct VcardProp {
    pub value: String,
    pub params: Vec<(String, String)>,
}

impl VcardProp {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn type_label(&self) -> Option<String> {
        self.param("TYPE").map(|s| s.to_ascii_lowercase())
    }
}

/// The property name of one unfolded line, `NAME[;params]:value`,
/// without the optional `group.` prefix RFC 6350 §3.3 allows — Apple
/// and Google both write `item1.EMAIL;…` for a labelled address, and a
/// matcher that keeps the prefix drops every one of those.
fn property_name(line: &str) -> &str {
    let head_end = line.find([':', ';']).unwrap_or(line.len());
    let head = &line[..head_end];
    head.rsplit_once('.').map_or(head, |(_, name)| name)
}

fn extract_property(vcard: &str, name: &str) -> Option<String> {
    let unfolded = unfold_vcard_lines(vcard);
    for line in unfolded.lines() {
        if property_name(line).eq_ignore_ascii_case(name) {
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
/// own status says deleted (404 / 410) — those land in
/// [`deleted_hrefs`] instead.
pub fn changed_contacts(ms: &Multistatus) -> HashMap<String, (Option<String>, String)> {
    let mut out = HashMap::new();
    for r in &ms.responses {
        if matches!(r.status, Some(404 | 410)) {
            continue;
        }
        if let Some(v) = &r.props.vcard {
            out.insert(r.href.clone(), (r.props.etag.clone(), v.clone()));
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
        .filter(|r| matches!(r.status, Some(404 | 410)))
        .map(|r| r.href.clone())
        .collect()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Multistatus {
        webdav::parse_multistatus(body).unwrap()
    }

    /// A grouped property (`item1.EMAIL`) is the same property. Every
    /// email-only card in a real Google export was rendering with no
    /// fields at all because the matcher compared the whole `item1.EMAIL`.
    #[test]
    fn grouped_properties_match_by_their_name() {
        let card = "BEGIN:VCARD\nitem1.EMAIL;TYPE=INTERNET:a@x.test\nitem1.X-ABLabel:\nEMAIL;TYPE=WORK:b@x.test\nitem2.TEL:+1-555\nEND:VCARD";
        let emails: Vec<String> = vcard_all(card, "EMAIL")
            .into_iter()
            .map(|p| p.value)
            .collect();
        assert_eq!(emails, vec!["a@x.test", "b@x.test"]);
        assert_eq!(
            vcard_all(card, "EMAIL")[0].params,
            vec![("TYPE".to_string(), "INTERNET".to_string())]
        );
        assert_eq!(extract_property(card, "TEL").as_deref(), Some("+1-555"));
        assert_eq!(
            extract_property(card, "X-ABLabel"),
            None,
            "blank value stays absent"
        );
    }

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
    fn parses_addressbook_home_set_uppercase_prefix() {
        let ms = parse(HOME_SET_APPLE);
        assert_eq!(ms.responses.len(), 1);
        assert_eq!(
            ms.responses[0].props.addressbook_home_set.as_deref(),
            Some("/123/carddavhome/")
        );
    }

    #[test]
    fn marks_resourcetype_addressbook_only_on_addressbook_collections() {
        let ms = parse(ADDRESSBOOK_LIST);
        assert_eq!(ms.responses.len(), 2);
        let abs: Vec<_> = ms
            .responses
            .iter()
            .filter(|r| r.props.is_addressbook)
            .map(|r| r.href.as_str())
            .collect();
        assert_eq!(abs, vec!["/dav/addressbooks/user/u%40example.com/Default/"]);
        // ctag still captured for the addressbook.
        let addr = ms
            .responses
            .iter()
            .find(|r| r.props.is_addressbook)
            .unwrap();
        assert_eq!(addr.props.ctag.as_deref(), Some("abc-1"));
        assert_eq!(addr.props.display_name.as_deref(), Some("Default"));
    }

    /// The card's CRs arrive as `&#13;` references between text runs.
    /// When the reader trimmed each run, the card lost its line feeds,
    /// so no UID was found and every contact was skipped.
    #[test]
    fn sync_collection_yields_changes_and_deletes_and_token() {
        let ms = parse(SYNC_COLLECTION);
        assert_eq!(
            ms.sync_token.as_deref(),
            Some("http://example.com/sync/4242")
        );
        let changed = changed_contacts(&ms);
        assert_eq!(changed.len(), 1);
        let (etag, vcard) = changed
            .get("/dav/addressbooks/user/u%40example.com/Default/abc.vcf")
            .unwrap();
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        assert!(vcard.contains("UID:abc"));
        assert_eq!(vcard_uid(vcard).as_deref(), Some("abc"), "{vcard:?}");
        let deleted = deleted_hrefs(&ms);
        assert_eq!(
            deleted,
            vec!["/dav/addressbooks/user/u%40example.com/Default/gone.vcf".to_string()]
        );
    }

    /// Fastmail's listing, verified against a live account: unprefixed
    /// `DAV:`, every value in CDATA, a 404 propstat after the 200 one.
    const ADDRESSBOOK_LIST_FASTMAIL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">
  <response>
    <href>/dav/addressbooks/user/picard@enterprise.test/Default/</href>
    <propstat>
      <prop>
        <resourcetype>
          <collection/>
          <card:addressbook/>
        </resourcetype>
        <displayname><![CDATA[Personal]]></displayname>
        <cs:getctag>1780602923-240</cs:getctag>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
    <propstat>
      <prop>
        <card:addressbook-description/>
      </prop>
      <status>HTTP/1.1 404 Not Found</status>
    </propstat>
  </response>
</multistatus>"#;

    /// Fastmail's `sync-collection` reply: the vCard arrives in CDATA
    /// with CRLF line ends.
    const SYNC_COLLECTION_FASTMAIL: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<multistatus xmlns=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">
  <response>
    <href>/dav/addressbooks/user/picard@enterprise.test/Default/riker.vcf</href>
    <propstat>
      <prop>
        <getetag>\"35513e3f\"</getetag>
        <card:address-data><![CDATA[BEGIN:VCARD\r\nVERSION:3.0\r\nUID:riker-1\r\nFN:William Riker\r\nEND:VCARD\r\n]]></card:address-data>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
  <sync-token>data:,1780602923-240</sync-token>
</multistatus>";

    /// Fastmail wraps every value in CDATA. Before the reader took CDATA
    /// as text, the addressbook had no name (so an `addressbooks` filter
    /// matched nothing) and every vCard was dropped as having no data.
    #[test]
    fn reads_fastmail_cdata_values() {
        let ms = parse(ADDRESSBOOK_LIST_FASTMAIL);
        let book = &ms.responses[0].props;
        assert!(book.is_addressbook);
        assert_eq!(book.display_name.as_deref(), Some("Personal"));
        assert_eq!(book.ctag.as_deref(), Some("1780602923-240"));

        let ms = parse(SYNC_COLLECTION_FASTMAIL);
        assert_eq!(ms.sync_token.as_deref(), Some("data:,1780602923-240"));
        let changed = changed_contacts(&ms);
        let (etag, vcard) = changed
            .get("/dav/addressbooks/user/picard@enterprise.test/Default/riker.vcf")
            .expect("the CDATA vCard is kept");
        assert_eq!(etag.as_deref(), Some("\"35513e3f\""));
        assert_eq!(
            vcard,
            "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:riker-1\r\nFN:William Riker\r\nEND:VCARD\r\n"
        );
        assert_eq!(vcard_uid(vcard).as_deref(), Some("riker-1"));
        assert_eq!(vcard_fn(vcard).as_deref(), Some("William Riker"));
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
}
