//! The bake's calendar check: every calendar source that mirrors its
//! calendars whole has its `render_occurrences` rows compared with what
//! the service itself lists for the same window — Google's
//! `events.list?singleEvents=true`, a CalDAV server's `<expand>` — so a
//! wrong repeat, a missed change or a time an hour off shows as a diff
//! in the report rather than waiting for someone to notice. And every
//! calendar source's occurrences are listed one per line, so a change in
//! what we list — ours or upstream's — is a line in the diff.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

/// Ids listed in a report, at most; the counts say the rest.
const LISTED: usize = 25;

/// One occurrence as either side has it: when it starts, in UTC, or its
/// date for an all-day one.
type Starts = BTreeMap<String, String>;

/// `(stanza, method, latchkey account)` for each calendar source with a
/// `render_occurrences` step whose ingest has no `since`/`until`. A
/// windowed source holds only the changes inside its window, so the
/// service would rightly disagree outside it.
pub fn whole_calendar_sources(cfg_text: &str) -> Vec<(String, String, String)> {
    let doc: toml::Table = toml::from_str(cfg_text).expect("parse config toml");
    let steps = doc
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let of = |group: &str, function: &str| {
        steps.iter().find(|s| {
            s.get("group").and_then(|g| g.as_str()) == Some(group)
                && s.get("function").and_then(|f| f.as_str()) == Some(function)
        })
    };
    let mut out = Vec::new();
    for g in doc
        .get("groups")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let (Some(id), Some("calendar")) = (
            g.get("id").and_then(|v| v.as_str()),
            g.get("type").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let (Some(ingest), Some(_)) = (of(id, "ingest"), of(id, "render_occurrences")) else {
            continue;
        };
        let params = ingest.get("params").and_then(|p| p.as_table());
        let Some((method, table)) = params.and_then(|p| {
            ["google", "fastmail", "caldav"]
                .iter()
                .find_map(|m| Some((m.to_string(), p.get(*m)?.as_table()?.clone())))
        }) else {
            continue;
        };
        if table.contains_key("since") || table.contains_key("until") {
            continue;
        }
        let account = params
            .and_then(|p| p.get("latchkey_settings"))
            .and_then(|l| l.get("account"))
            .and_then(|a| a.as_str())
            .unwrap_or_default()
            .to_string();
        out.push((id.to_string(), method, account));
    }
    out
}

/// Every occurrence `stanza` lists, one JSON object a line, by start:
/// the window first, the events that could not be listed last. The
/// payload is left out: Google moves `etag` and `updated` on events
/// nothing else changed.
pub fn listing(data_root: &Path, stanza: &str) -> String {
    use sqlx::Row;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the occurrences listing");
    rt.block_on(async {
        let path = data_root.join(stanza).join("render_occurrences/occurrences.doltlite_db");
        let pool = super::open_readonly(&path).await;
        let mut lines = Vec::new();
        let w = sqlx::query("SELECT from_utc, until_utc FROM occurrences_window")
            .fetch_one(&pool)
            .await
            .expect("read occurrences_window");
        lines.push(json!({ "window": [w.get::<String, _>("from_utc"), w.get::<String, _>("until_utc")] }));
        let rows = sqlx::query(
            "SELECT calendar_id, instance_id, series_id, start_time, end_time, status, all_day, title \
             FROM occurrences ORDER BY start_utc, calendar_id, instance_id",
        )
        .fetch_all(&pool)
        .await
        .expect("read occurrences");
        for r in &rows {
            lines.push(json!({
                "calendar": r.get::<String, _>("calendar_id"),
                "id": r.get::<String, _>("instance_id"),
                "series": r.get::<Option<String>, _>("series_id"),
                "start": r.get::<String, _>("start_time"),
                "end": r.get::<Option<String>, _>("end_time"),
                "status": r.get::<String, _>("status"),
                "all_day": r.get::<i64, _>("all_day") != 0,
                "title": r.get::<Option<String>, _>("title"),
            }));
        }
        let unlisted = sqlx::query("SELECT event_row, reason FROM unlisted ORDER BY event_row, reason")
            .fetch_all(&pool)
            .await
            .expect("read unlisted");
        for r in &unlisted {
            lines.push(json!({
                "unlisted": r.get::<String, _>("event_row"),
                "reason": r.get::<String, _>("reason"),
            }));
        }
        pool.close().await;
        let mut out = lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        out.push('\n');
        out
    })
}

/// The report for one source: what each side lists, and every
/// difference by id.
pub fn compare(data_root: &Path, stanza: &str, method: &str, account: &str) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the occurrences check");
    let (ours, window, calendars) = rt.block_on(read_ours(data_root, stanza, method));
    let (theirs, no_recurrence_id) = match method {
        "google" => (
            google_listing(account, &calendars, &window),
            BTreeSet::new(),
        ),
        _ => caldav_listing(account, &calendars, &window),
    };
    report(&ours, &theirs, &no_recurrence_id, &window)
}

/// A CalDAV server may leave `RECURRENCE-ID` off one instance of an
/// expanded series (Fastmail does, for a handful); such an instance is
/// matched to ours by its `UID` and start.
fn report(
    ours: &Starts,
    theirs: &Starts,
    no_rid: &BTreeSet<String>,
    window: &(String, String),
) -> Value {
    let mut only_ours: BTreeSet<&String> =
        ours.keys().filter(|k| !theirs.contains_key(*k)).collect();
    let mut only_theirs: BTreeSet<&String> =
        theirs.keys().filter(|k| !ours.contains_key(*k)).collect();
    let mut matched_without_rid = 0usize;
    for uid in no_rid {
        let Some(start) = theirs.get(uid) else {
            continue;
        };
        let twin = only_ours
            .iter()
            .find(|k| k.starts_with(&format!("{uid}_")) && ours.get(**k) == Some(start))
            .copied();
        if let Some(twin) = twin {
            only_ours.remove(twin);
            only_theirs.remove(uid);
            matched_without_rid += 1;
        }
    }
    let start_differs: Vec<Value> = ours
        .iter()
        .filter_map(|(k, s)| {
            let t = theirs.get(k)?;
            (t != s).then(|| json!({ "id": k, "service": t, "ours": s }))
        })
        .collect();
    let first = |set: &BTreeSet<&String>| {
        set.iter()
            .take(LISTED)
            .cloned()
            .cloned()
            .collect::<Vec<_>>()
    };
    json!({
        "window": [window.0, window.1],
        "service_lists": theirs.len(),
        "we_list": ours.len(),
        "matched_without_recurrence_id": matched_without_rid,
        "only_service_count": only_theirs.len(),
        "only_service": first(&only_theirs),
        "only_ours_count": only_ours.len(),
        "only_ours": first(&only_ours),
        "start_differs_count": start_differs.len(),
        "start_differs": start_differs.into_iter().take(LISTED).collect::<Vec<_>>(),
    })
}

/// Our rows, the window they cover, and the calendars they came from:
/// `(calendar id, CalDAV collection URL)`, the URL empty for Google.
async fn read_ours(
    data_root: &Path,
    stanza: &str,
    method: &str,
) -> (Starts, (String, String), Vec<(String, String)>) {
    use sqlx::Row;
    let occ = super::open_readonly(
        &data_root
            .join(stanza)
            .join("render_occurrences/occurrences.doltlite_db"),
    )
    .await;
    let rows = sqlx::query("SELECT instance_id, start_utc, start_time, all_day FROM occurrences")
        .fetch_all(&occ)
        .await
        .expect("read occurrences");
    let ours: Starts = rows
        .iter()
        .map(|r| {
            let id: String = r.get("instance_id");
            let start: String = match r.get::<i64, _>("all_day") {
                0 => r.get("start_utc"),
                _ => format!("date:{}", r.get::<String, _>("start_time")),
            };
            (id, start)
        })
        .collect();
    let w = sqlx::query("SELECT from_utc, until_utc FROM occurrences_window")
        .fetch_one(&occ)
        .await
        .expect("read occurrences_window");
    let window = (w.get("from_utc"), w.get("until_utc"));
    occ.close().await;

    let raw =
        super::open_readonly(&data_root.join(stanza).join("ingest/entities.doltlite_db")).await;
    let sql = match method {
        "google" => "SELECT DISTINCT calendar_id AS id, '' AS url FROM google_events",
        _ => {
            "SELECT DISTINCT o.calendar_id AS id, a.server_url || '|' || c.href AS url \
             FROM ics_objects o JOIN calendars c ON c.id = o.calendar_id JOIN accounts a"
        }
    };
    let calendars = sqlx::query(sql)
        .fetch_all(&raw)
        .await
        .expect("read calendars")
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("id"),
                collection_url(&r.get::<String, _>("url")),
            )
        })
        .collect();
    raw.close().await;
    (ours, window, calendars)
}

fn collection_url(server_and_href: &str) -> String {
    let Some((server, href)) = server_and_href.split_once('|') else {
        return String::new();
    };
    if href.starts_with("http") {
        return href.to_string();
    }
    let origin: String = server.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    format!("{origin}{href}")
}

fn latchkey(account: &str, args: &[&str]) -> String {
    let mut cmd = datalib_runtime::node_runtime::latchkey_command()
        .unwrap_or_else(|e| panic!("no latchkey for the occurrences check: {e}"));
    if !account.is_empty() {
        cmd.args(["--account", account]);
    }
    let out = cmd
        .arg("curl")
        .arg("-s")
        .args(args)
        .output()
        .expect("spawn latchkey");
    assert!(
        out.status.success(),
        "latchkey curl failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn google_listing(
    account: &str,
    calendars: &[(String, String)],
    window: &(String, String),
) -> Starts {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    let mut out = Starts::new();
    for (cid, _) in calendars {
        let mut token: Option<String> = None;
        loop {
            let mut url = format!(
                "https://www.googleapis.com/calendar/v3/calendars/{}/events?singleEvents=true&maxResults=2500&timeMin={}&timeMax={}",
                enc(cid), enc(&window.0), enc(&window.1)
            );
            if let Some(t) = &token {
                url.push_str(&format!("&pageToken={}", enc(t)));
            }
            let page: Value =
                serde_json::from_str(&latchkey(account, &[&url])).expect("Google answered JSON");
            let items = page
                .get("items")
                .and_then(|i| i.as_array())
                .unwrap_or_else(|| panic!("Google listing without items: {page}"));
            for e in items {
                let id = e["id"].as_str().unwrap_or_default().to_string();
                out.insert(id, google_start(&e["start"]));
            }
            token = page
                .get("nextPageToken")
                .and_then(|t| t.as_str())
                .map(str::to_string);
            if token.is_none() {
                break;
            }
        }
    }
    out
}

fn google_start(t: &Value) -> String {
    if let Some(d) = t.get("date").and_then(|d| d.as_str()) {
        return format!("date:{d}");
    }
    let dt = t
        .get("dateTime")
        .and_then(|d| d.as_str())
        .unwrap_or_default();
    utc_text(dt)
}

/// An RFC 3339 time as `YYYY-MM-DDTHH:MM:SSZ`, the way `start_utc` is.
fn utc_text(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn caldav_listing(
    account: &str,
    calendars: &[(String, String)],
    window: &(String, String),
) -> (Starts, BTreeSet<String>) {
    let basic = |s: &str| s.replace(['-', ':'], "");
    let (from, until) = (basic(&window.0), basic(&window.1));
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:calendar-query xmlns:D=\"DAV:\" \
         xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><C:calendar-data><C:expand start=\"{from}\" \
         end=\"{until}\"/></C:calendar-data></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\">\
         <C:comp-filter name=\"VEVENT\"><C:time-range start=\"{from}\" end=\"{until}\"/></C:comp-filter>\
         </C:comp-filter></C:filter></C:calendar-query>"
    );
    let mut out = Starts::new();
    let mut no_rid = BTreeSet::new();
    for (_, url) in calendars {
        let xml = latchkey(
            account,
            &[
                "-X",
                "REPORT",
                "-H",
                "Depth: 1",
                "-H",
                "Content-Type: application/xml",
                url,
                "--data-binary",
                &body,
            ],
        );
        let unfolded = xml
            .replace("\r\n ", "")
            .replace("\n ", "")
            .replace("\r\n\t", "")
            .replace("\n\t", "");
        for block in unfolded.split("BEGIN:VEVENT").skip(1) {
            let block = block.split("END:VEVENT").next().unwrap_or_default();
            let prop = |name: &str| {
                block.lines().find_map(|l| {
                    let (head, value) = l.trim_end_matches('\r').split_once(':')?;
                    (head.split(';').next() == Some(name)).then(|| value.trim().to_string())
                })
            };
            if prop("STATUS").is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED")) {
                continue;
            }
            let Some(uid) = prop("UID") else { continue };
            let start = prop("DTSTART").map(|v| ical_start(&v)).unwrap_or_default();
            match prop("RECURRENCE-ID") {
                Some(rid) => out.insert(format!("{uid}_{rid}"), start),
                None => {
                    no_rid.insert(uid.clone());
                    out.insert(uid, start)
                }
            };
        }
    }
    (out, no_rid)
}

/// An expanded instance's `DTSTART`, which RFC 4791 has in UTC, or a date.
fn ical_start(v: &str) -> String {
    if v.len() == 8 {
        return format!("date:{}-{}-{}", &v[0..4], &v[4..6], &v[6..8]);
    }
    let b = v.trim_end_matches(['Z', 'z']);
    if b.len() < 15 {
        return v.to_string();
    }
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &b[0..4],
        &b[4..6],
        &b[6..8],
        &b[9..11],
        &b[11..13],
        &b[13..15]
    )
}
