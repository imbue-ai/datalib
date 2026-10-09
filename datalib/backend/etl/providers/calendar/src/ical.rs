//! A small iCalendar (RFC 5545) reader: an `.ics` file split one event at
//! a time, over the content-line grammar it shares with vCard
//! (`datalib_etl::content_line`). Enough for what a calendar carries; it
//! does not validate.

pub use datalib_etl::content_line::{parse, parse_line, unfold, Component, Property};

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
