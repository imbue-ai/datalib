//! What CalDAV asks a WebDAV server and reads out of its replies; the
//! requests and the `multistatus` walk are [`datalib_etl::dav`]'s.

use quick_xml::events::BytesStart;

use datalib_etl::dav::{self as webdav, DavProps};
use datalib_etl::http::{HttpMethod, HttpRequest, HttpService, LatchkeySettings};

pub use datalib_etl::dav::{absolutize, origin, DavError, BODY_CURRENT_USER_PRINCIPAL};

pub const HTTP_SERVICE: HttpService = HttpService::Caldav;

pub type DavResponse = webdav::DavResponse<CalendarProps>;
pub type Multistatus = webdav::Multistatus<CalendarProps>;

/// A property is `Some` only when the server returned it with a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarProps {
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    /// `calendar-timezone`: a whole `VCALENDAR` holding one `VTIMEZONE`.
    pub calendar_timezone: Option<String>,
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

impl DavProps for CalendarProps {
    fn leaf(&mut self, name: &str, parent: &str, text: String) {
        let trimmed = text.trim();
        let some = || (!trimmed.is_empty()).then(|| trimmed.to_string());
        match (name, parent) {
            ("href", "calendar-home-set") => self.calendar_home_set = some(),
            ("href", "calendar-user-address-set") => self.user_addresses.extend(some()),
            ("getetag", _) => self.etag = some(),
            ("displayname", _) => self.display_name = some(),
            ("calendar-description", _) => self.description = some(),
            ("calendar-color", _) => self.color = some(),
            ("calendar-timezone", _) => self.calendar_timezone = some(),
            // Not trimmed: the object is what the server stored.
            ("calendar-data", _) if !trimmed.is_empty() => self.calendar_data = Some(text),
            _ => {}
        }
    }

    fn empty(&mut self, name: &str, parent: &str, element: &BytesStart<'_>) {
        if name == "calendar" && parent == "resourcetype" {
            self.is_calendar = true;
        }
        if name == "comp" && parent == "supported-calendar-component-set" {
            // A component name is plain ASCII: nothing to unescape.
            if let Ok(Some(attr)) = element.try_get_attribute("name") {
                self.components
                    .push(String::from_utf8_lossy(&attr.value).to_ascii_uppercase());
            }
        }
    }
}

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

pub fn body_sync_collection(prev_token: &str) -> String {
    webdav::body_sync_collection(
        prev_token,
        r#"xmlns:C="urn:ietf:params:xml:ns:caldav""#,
        "C:calendar-data",
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

/// RFC 4791 `calendar-multiget`, for the resources a listing named
/// without their data.
pub fn body_multiget(hrefs: &[String]) -> String {
    let hrefs: String = hrefs
        .iter()
        .map(|h| format!("  <href>{}</href>\n", webdav::escape_xml(h)))
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

pub async fn propfind(
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    webdav::propfind(HTTP_SERVICE, url, depth, body, latchkey).await
}

pub async fn report(
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> Result<Multistatus, DavError> {
    webdav::report(HTTP_SERVICE, url, depth, body, latchkey).await
}

pub fn http_request(
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    latchkey: &LatchkeySettings,
) -> HttpRequest {
    webdav::http_request(HTTP_SERVICE, method, url, depth, body, latchkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Multistatus {
        webdav::parse_multistatus(body).unwrap()
    }

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
        let ms = parse(LISTING);
        assert_eq!(ms.responses.len(), 3);
        let cals: Vec<&DavResponse> = ms
            .responses
            .iter()
            .filter(|r| r.props.is_calendar)
            .collect();
        assert_eq!(cals.len(), 1);
        let bridge = &cals[0].props;
        assert_eq!(bridge.display_name.as_deref(), Some("Bridge Duty"));
        assert_eq!(bridge.color.as_deref(), Some("#16A765"));
        assert_eq!(bridge.components, vec!["VEVENT"]);
        assert!(bridge
            .calendar_timezone
            .as_deref()
            .unwrap()
            .contains("TZID:America/Los_Angeles"));
        assert_eq!(cals[0].status, None);
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
        let ms = parse(body);
        assert_eq!(ms.sync_token.as_deref(), Some("data:,42"));
        assert_eq!(ms.responses[0].props.etag.as_deref(), Some("\"e1\""));
        assert!(ms.responses[0]
            .props
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
        let ms = parse(body);
        let r = &ms.responses[0];
        assert_eq!(r.href, "/p/");
        assert_eq!(
            r.props.calendar_home_set.as_deref(),
            Some("/dav/calendars/user/picard@enterprise.test/")
        );
        assert_eq!(
            r.props.user_addresses,
            vec!["mailto:picard@enterprise.test", "/p/"]
        );
    }
}
