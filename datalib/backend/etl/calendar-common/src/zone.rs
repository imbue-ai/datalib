//! The zone a calendar's `TZID` names. RFC 5545 lets a `TZID` be any
//! string; besides IANA's names two kinds turn up, each meaning one thing
//! wherever it appears: Windows' names, which Outlook and Exchange write
//! (`Pacific Standard Time`), and a bare offset (`GMT-0700`), whose
//! `VTIMEZONE` is that offset all year. Any other name is unknown, never
//! guessed.

use datalib_time::NamedZone;

pub fn named(tzid: &str) -> Option<NamedZone> {
    let tzid = tzid.trim();
    NamedZone::get(tzid)
        .or_else(|| windows(tzid))
        .or_else(|| offset(tzid))
}

fn windows(name: &str) -> Option<NamedZone> {
    let (_, iana) = WINDOWS.iter().find(|(w, _)| w.eq_ignore_ascii_case(name))?;
    NamedZone::get(iana)
}

/// `GMT-0700`, `UTC+05:30`, `GMT+8`: east of UTC, as ISO 8601 signs it
/// (not POSIX's or `Etc/GMT+7`'s reversed sign).
fn offset(name: &str) -> Option<NamedZone> {
    let prefix = name.get(..3)?;
    if !(prefix.eq_ignore_ascii_case("GMT") || prefix.eq_ignore_ascii_case("UTC")) {
        return None;
    }
    let rest = &name[3..];
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let digits = digits.replacen(':', "", 1);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        1 | 2 => (digits.as_str(), "0"),
        4 => digits.split_at(2),
        _ => return None,
    };
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 14 || m >= 60 {
        return None;
    }
    NamedZone::fixed(sign * (h * 3600 + m * 60))
}

/// CLDR's `windowsZones.xml` (release 47), each name's territory-`001`
/// zone. © Unicode, Inc., Unicode License v3.
const WINDOWS: &[(&str, &str)] = &[
    ("AUS Central Standard Time", "Australia/Darwin"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("Afghanistan Standard Time", "Asia/Kabul"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Aleutian Standard Time", "America/Adak"),
    ("Altai Standard Time", "Asia/Barnaul"),
    ("Arab Standard Time", "Asia/Riyadh"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Arabic Standard Time", "Asia/Baghdad"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Astrakhan Standard Time", "Europe/Astrakhan"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Aus Central W. Standard Time", "Australia/Eucla"),
    ("Azerbaijan Standard Time", "Asia/Baku"),
    ("Azores Standard Time", "Atlantic/Azores"),
    ("Bahia Standard Time", "America/Bahia"),
    ("Bangladesh Standard Time", "Asia/Dhaka"),
    ("Belarus Standard Time", "Europe/Minsk"),
    ("Bougainville Standard Time", "Pacific/Bougainville"),
    ("Canada Central Standard Time", "America/Regina"),
    ("Cape Verde Standard Time", "Atlantic/Cape_Verde"),
    ("Caucasus Standard Time", "Asia/Yerevan"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Central Asia Standard Time", "Asia/Bishkek"),
    ("Central Brazilian Standard Time", "America/Cuiaba"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("Central Pacific Standard Time", "Pacific/Guadalcanal"),
    ("Central Standard Time", "America/Chicago"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Chatham Islands Standard Time", "Pacific/Chatham"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Cuba Standard Time", "America/Havana"),
    ("Dateline Standard Time", "Etc/GMT+12"),
    ("E. Africa Standard Time", "Africa/Nairobi"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Easter Island Standard Time", "Pacific/Easter"),
    ("Eastern Standard Time", "America/New_York"),
    ("Eastern Standard Time (Mexico)", "America/Cancun"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("Ekaterinburg Standard Time", "Asia/Yekaterinburg"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("Fiji Standard Time", "Pacific/Fiji"),
    ("GMT Standard Time", "Europe/London"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Georgian Standard Time", "Asia/Tbilisi"),
    ("Greenland Standard Time", "America/Godthab"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("Haiti Standard Time", "America/Port-au-Prince"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("India Standard Time", "Asia/Calcutta"),
    ("Iran Standard Time", "Asia/Tehran"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Jordan Standard Time", "Asia/Amman"),
    ("Kaliningrad Standard Time", "Europe/Kaliningrad"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Libya Standard Time", "Africa/Tripoli"),
    ("Line Islands Standard Time", "Pacific/Kiritimati"),
    ("Lord Howe Standard Time", "Australia/Lord_Howe"),
    ("Magadan Standard Time", "Asia/Magadan"),
    ("Magallanes Standard Time", "America/Punta_Arenas"),
    ("Marquesas Standard Time", "Pacific/Marquesas"),
    ("Mauritius Standard Time", "Indian/Mauritius"),
    ("Middle East Standard Time", "Asia/Beirut"),
    ("Montevideo Standard Time", "America/Montevideo"),
    ("Morocco Standard Time", "Africa/Casablanca"),
    ("Mountain Standard Time", "America/Denver"),
    ("Mountain Standard Time (Mexico)", "America/Mazatlan"),
    ("Myanmar Standard Time", "Asia/Rangoon"),
    ("N. Central Asia Standard Time", "Asia/Novosibirsk"),
    ("Namibia Standard Time", "Africa/Windhoek"),
    ("Nepal Standard Time", "Asia/Katmandu"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("Norfolk Standard Time", "Pacific/Norfolk"),
    ("North Asia East Standard Time", "Asia/Irkutsk"),
    ("North Asia Standard Time", "Asia/Krasnoyarsk"),
    ("North Korea Standard Time", "Asia/Pyongyang"),
    ("Omsk Standard Time", "Asia/Omsk"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Pacific Standard Time (Mexico)", "America/Tijuana"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("Paraguay Standard Time", "America/Asuncion"),
    ("Qyzylorda Standard Time", "Asia/Qyzylorda"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Russia Time Zone 10", "Asia/Srednekolymsk"),
    ("Russia Time Zone 11", "Asia/Kamchatka"),
    ("Russia Time Zone 3", "Europe/Samara"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("SA Eastern Standard Time", "America/Cayenne"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("SA Western Standard Time", "America/La_Paz"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("Saint Pierre Standard Time", "America/Miquelon"),
    ("Sakhalin Standard Time", "Asia/Sakhalin"),
    ("Samoa Standard Time", "Pacific/Apia"),
    ("Sao Tome Standard Time", "Africa/Sao_Tome"),
    ("Saratov Standard Time", "Europe/Saratov"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("South Sudan Standard Time", "Africa/Juba"),
    ("Sri Lanka Standard Time", "Asia/Colombo"),
    ("Sudan Standard Time", "Africa/Khartoum"),
    ("Syria Standard Time", "Asia/Damascus"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Tasmania Standard Time", "Australia/Hobart"),
    ("Tocantins Standard Time", "America/Araguaina"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Tomsk Standard Time", "Asia/Tomsk"),
    ("Tonga Standard Time", "Pacific/Tongatapu"),
    ("Transbaikal Standard Time", "Asia/Chita"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Turks And Caicos Standard Time", "America/Grand_Turk"),
    ("US Eastern Standard Time", "America/Indianapolis"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("UTC", "Etc/UTC"),
    ("UTC+12", "Etc/GMT-12"),
    ("UTC+13", "Etc/GMT-13"),
    ("UTC-02", "Etc/GMT+2"),
    ("UTC-08", "Etc/GMT+8"),
    ("UTC-09", "Etc/GMT+9"),
    ("UTC-11", "Etc/GMT+11"),
    ("Ulaanbaatar Standard Time", "Asia/Ulaanbaatar"),
    ("Venezuela Standard Time", "America/Caracas"),
    ("Vladivostok Standard Time", "Asia/Vladivostok"),
    ("Volgograd Standard Time", "Europe/Volgograd"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("W. Mongolia Standard Time", "Asia/Hovd"),
    ("West Asia Standard Time", "Asia/Tashkent"),
    ("West Bank Standard Time", "Asia/Hebron"),
    ("West Pacific Standard Time", "Pacific/Port_Moresby"),
    ("Yakutsk Standard Time", "Asia/Yakutsk"),
    ("Yukon Standard Time", "America/Whitehorse"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn at(tzid: &str, local: &str) -> String {
        let local = NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M").unwrap();
        named(tzid)
            .unwrap_or_else(|| panic!("{tzid} did not resolve"))
            .from_local(&local)
            .single()
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn every_windows_name_reaches_a_zone_the_database_holds() {
        let missing: Vec<_> = WINDOWS
            .iter()
            .filter(|(_, iana)| NamedZone::get(iana).is_none())
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// Outlook writes `TZID=Pacific Standard Time` with the US rules in
    /// its `VTIMEZONE`; it was refused as unknown.
    #[test]
    fn a_windows_name_keeps_its_daylight_saving() {
        assert_eq!(
            at("Pacific Standard Time", "2026-01-05T09:00"),
            "2026-01-05T09:00:00-08:00"
        );
        assert_eq!(
            at("Pacific Standard Time", "2026-07-06T09:00"),
            "2026-07-06T09:00:00-07:00"
        );
        assert_eq!(
            at("india standard time", "2026-07-06T09:00"),
            "2026-07-06T09:00:00+05:30"
        );
    }

    /// `TZID=GMT-0700` comes with a `VTIMEZONE` that is -07:00 all year;
    /// it was refused as unknown.
    #[test]
    fn a_gmt_offset_is_that_offset_all_year() {
        assert_eq!(
            at("GMT-0700", "2026-01-05T09:00"),
            "2026-01-05T09:00:00-07:00"
        );
        assert_eq!(
            at("GMT-0700", "2026-07-06T09:00"),
            "2026-07-06T09:00:00-07:00"
        );
        assert_eq!(
            at("UTC+05:30", "2026-07-06T09:00"),
            "2026-07-06T09:00:00+05:30"
        );
        assert_eq!(at("GMT+8", "2026-07-06T09:00"), "2026-07-06T09:00:00+08:00");
    }

    #[test]
    fn anything_else_is_unknown() {
        for name in [
            "Customized Time Zone",
            "GMT-0760",
            "GMT+1500",
            "GMT 0700",
            "GMT-07000",
            "GMT-07:0",
        ] {
            assert!(named(name).is_none(), "{name}");
        }
    }
}
