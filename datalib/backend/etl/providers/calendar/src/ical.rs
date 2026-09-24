//! A small iCalendar (RFC 5545) reader: content lines into a component
//! tree, and back out of an `.ics` file one event at a time. Enough for
//! what a calendar carries; it does not validate.

/// One `NAME;PARAM=V:value` line, unfolded. The value is as written,
/// escapes and all; [`Property::text`] undoes the TEXT escapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Property {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// The value with RFC 5545 §3.3.11 TEXT escapes undone.
    pub fn text(&self) -> String {
        unescape_text(&self.value)
    }
}

/// `BEGIN:NAME` … `END:NAME`, with its properties and the components
/// nested inside it, both in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.props
            .iter()
            .filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// A property's TEXT value, `None` when absent or blank.
    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name)
            .map(Property::text)
            .filter(|s| !s.trim().is_empty())
    }
}

/// Every top-level component in `text`. A line outside any component,
/// and an `END` that closes nothing, are skipped: exports in the wild
/// carry both.
pub fn parse(text: &str) -> Vec<Component> {
    let mut top: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    for line in unfold(text) {
        let Some(prop) = parse_line(&line) else {
            continue;
        };
        if prop.name.eq_ignore_ascii_case("BEGIN") {
            stack.push(Component {
                name: prop.value.trim().to_ascii_uppercase(),
                ..Default::default()
            });
        } else if prop.name.eq_ignore_ascii_case("END") {
            let name = prop.value.trim().to_ascii_uppercase();
            if !stack.iter().any(|c| c.name == name) {
                continue;
            }
            // Close anything left open inside it: a missing END is a
            // truncated component, not a reason to lose its parent.
            while let Some(done) = stack.pop() {
                let closes = done.name == name;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => top.push(done),
                }
                if closes {
                    break;
                }
            }
        } else if let Some(open) = stack.last_mut() {
            open.props.push(prop);
        }
    }
    while let Some(done) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(done),
            None => top.push(done),
        }
    }
    top
}

/// Content lines with RFC 5545 §3.1 folding undone: a line that starts
/// with a space or tab continues the one before it, minus that one
/// character. Any line ending is accepted.
pub fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match (line.chars().next(), out.last_mut()) {
            (Some(' ' | '\t'), Some(prev)) => prev.push_str(&line[1..]),
            _ if line.is_empty() => {}
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// One unfolded line, or `None` when it has no `:`. A parameter value
/// may be quoted, and a quoted one may hold `;`, `:` and `,`.
pub fn parse_line(line: &str) -> Option<Property> {
    let mut name_end = None;
    let mut value_start = None;
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes && name_end.is_none() => name_end = Some(i),
            ':' if !in_quotes => {
                value_start = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = value_start?;
    let name_end = name_end.unwrap_or(colon);
    let name = line[..name_end].trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = if name_end < colon {
        split_params(&line[name_end + 1..colon])
    } else {
        Vec::new()
    };
    Some(Property {
        name,
        params,
        value: line[colon + 1..].to_string(),
    })
}

fn split_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chunk = String::new();
    let mut in_quotes = false;
    let mut chunks = Vec::new();
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                chunk.push(c);
            }
            ';' if !in_quotes => chunks.push(std::mem::take(&mut chunk)),
            _ => chunk.push(c),
        }
    }
    chunks.push(chunk);
    for chunk in chunks {
        if let Some((k, v)) = chunk.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            out.push((k.trim().to_ascii_uppercase(), v.to_string()));
        }
    }
    out
}

pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The `UID` of the first `VEVENT` in an iCalendar object. A CalDAV
/// resource holds one event and its changed occurrences, which all
/// share it.
pub fn first_event_uid(ics: &str) -> Option<String> {
    parse(ics)
        .iter()
        .flat_map(|cal| cal.children_named("VEVENT"))
        .find_map(|ev| ev.text("UID"))
        .map(|s| s.trim().to_string())
}

/// One event of an `.ics` file: its `UID`, and a standalone
/// `VCALENDAR` holding every `VEVENT` that carries it (the series and
/// its changed occurrences) plus the `VTIMEZONE`s they name — the same
/// shape a CalDAV server stores per resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitEvent {
    pub uid: String,
    pub ics: String,
}

/// What an `.ics` file holds besides its events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitFile {
    /// `X-WR-CALNAME`, the name Google and Apple give a calendar export.
    pub calendar_name: Option<String>,
    /// `X-WR-TIMEZONE`: the zone a floating time in this file is in.
    pub time_zone: Option<String>,
    pub events: Vec<SplitEvent>,
    /// `VEVENT`s with no `UID`, which cannot be told apart run to run.
    pub events_without_uid: usize,
}

/// Split an exported calendar into one object per `UID`, keeping each
/// event's lines exactly as the file wrote them.
pub fn split_file(text: &str) -> SplitFile {
    let lines = unfold(text);
    let mut out = SplitFile::default();
    let mut timezones: Vec<(String, Vec<String>)> = Vec::new();
    let mut by_uid: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    let mut depth_in: Option<(&str, Vec<String>)> = None;
    let mut nested = 0usize;
    let mut prodid: Option<String> = None;
    for line in &lines {
        let prop = parse_line(line);
        let (name, value) = prop
            .as_ref()
            .map(|p| (p.name.as_str(), p.value.trim()))
            .unwrap_or(("", ""));
        if let Some((kind, buf)) = depth_in.as_mut() {
            buf.push(line.clone());
            if name == "BEGIN" {
                nested += 1;
            } else if name == "END" && nested > 0 {
                nested -= 1;
            } else if name == "END" && value.eq_ignore_ascii_case(kind) {
                let block = std::mem::take(buf);
                let kind = *kind;
                depth_in = None;
                let comp = parse(&block.join("\n"));
                let Some(comp) = comp.first() else { continue };
                if kind == "VTIMEZONE" {
                    if let Some(tzid) = comp.text("TZID") {
                        timezones.push((tzid, block));
                    }
                    continue;
                }
                let Some(uid) = comp.text("UID").map(|u| u.trim().to_string()) else {
                    out.events_without_uid += 1;
                    continue;
                };
                let tzids: Vec<String> = tzids_named(comp);
                match by_uid.iter_mut().find(|(u, _, _)| *u == uid) {
                    Some((_, blocks, zones)) => {
                        blocks.extend(block);
                        zones.extend(tzids);
                    }
                    None => by_uid.push((uid, block, tzids)),
                }
            }
            continue;
        }
        match (name, value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VEVENT") => depth_in = Some(("VEVENT", vec![line.clone()])),
            ("BEGIN", "VTIMEZONE") => depth_in = Some(("VTIMEZONE", vec![line.clone()])),
            ("X-WR-CALNAME", _) => {
                out.calendar_name = prop.as_ref().map(Property::text).filter(|s| !s.is_empty())
            }
            ("X-WR-TIMEZONE", _) => out.time_zone = Some(value.to_string()),
            ("PRODID", _) if prodid.is_none() => prodid = Some(value.to_string()),
            _ => {}
        }
    }
    let prodid = prodid.unwrap_or_else(|| "-//datalib//calendar//EN".to_string());
    for (uid, blocks, zones) in by_uid {
        let mut ics = vec![
            "BEGIN:VCALENDAR".to_string(),
            "VERSION:2.0".to_string(),
            format!("PRODID:{prodid}"),
        ];
        for (tzid, block) in &timezones {
            if zones.iter().any(|z| z == tzid) {
                ics.extend(block.iter().cloned());
            }
        }
        ics.extend(blocks);
        ics.push("END:VCALENDAR".to_string());
        ics.push(String::new());
        out.events.push(SplitEvent {
            uid,
            ics: ics.join("\r\n"),
        });
    }
    out
}

fn tzids_named(comp: &Component) -> Vec<String> {
    let mut out: Vec<String> = comp
        .props
        .iter()
        .filter_map(|p| p.param("TZID").map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfolds_and_parses_quoted_params() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ncc-1701-d\r\nSUMMARY:Senior staff\r\n  meeting\r\nATTENDEE;CN=\"Riker, William\";PARTSTAT=ACCEPTED:mailto:riker@enterprise.test\r\nDESCRIPTION:Agenda:\\nSaucer separation\\, drill\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let cal = parse(text);
        assert_eq!(cal.len(), 1);
        let ev = cal[0].children_named("VEVENT").next().unwrap();
        assert_eq!(ev.text("SUMMARY").as_deref(), Some("Senior staff meeting"));
        let att = ev.prop("ATTENDEE").unwrap();
        assert_eq!(att.param("cn"), Some("Riker, William"));
        assert_eq!(att.value, "mailto:riker@enterprise.test");
        assert_eq!(
            ev.text("DESCRIPTION").as_deref(),
            Some("Agenda:\nSaucer separation, drill")
        );
    }

    #[test]
    fn a_missing_end_does_not_lose_the_parent() {
        let cal = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:a\nBEGIN:VALARM\nACTION:DISPLAY\nEND:VEVENT\nEND:VCALENDAR\n");
        let ev = cal[0].children_named("VEVENT").next().unwrap();
        assert_eq!(ev.text("UID").as_deref(), Some("a"));
        assert_eq!(ev.children_named("VALARM").count(), 1);
    }

    #[test]
    fn splits_a_file_by_uid_keeping_overrides_and_their_zones() {
        let file = "BEGIN:VCALENDAR\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\nX-WR-CALNAME:Bridge\r\nX-WR-TIMEZONE:America/Los_Angeles\r\n\
BEGIN:VTIMEZONE\r\nTZID:America/Los_Angeles\r\nBEGIN:STANDARD\r\nTZOFFSETFROM:-0700\r\nTZOFFSETTO:-0800\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Paris\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:staff\r\nDTSTART;TZID=America/Los_Angeles:23700101T090000\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:poker\r\nDTSTART:23700102T020000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:staff\r\nRECURRENCE-ID;TZID=America/Los_Angeles:23700108T090000\r\nDTSTART;TZID=America/Los_Angeles:23700108T100000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:no uid\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let split = split_file(file);
        assert_eq!(split.calendar_name.as_deref(), Some("Bridge"));
        assert_eq!(split.time_zone.as_deref(), Some("America/Los_Angeles"));
        assert_eq!(split.events_without_uid, 1);
        let uids: Vec<&str> = split.events.iter().map(|e| e.uid.as_str()).collect();
        assert_eq!(uids, vec!["staff", "poker"]);
        let staff = parse(&split.events[0].ics);
        assert_eq!(staff[0].children_named("VEVENT").count(), 2);
        assert_eq!(staff[0].children_named("VTIMEZONE").count(), 1);
        let poker = parse(&split.events[1].ics);
        assert_eq!(poker[0].children_named("VTIMEZONE").count(), 0);
        assert_eq!(
            first_event_uid(&split.events[0].ics).as_deref(),
            Some("staff")
        );
    }
}
