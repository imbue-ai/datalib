//! When a device's reading really happened. A device's `Timestamp` is not a
//! real epoch: it is its local clock read as if it were UTC
//! (`INGEST.md` § "A device's clock"). This turns what it wrote into real
//! time and says when that cannot be trusted. Pure: the download stores the
//! changes and reports the trouble, and the render converts.

use std::collections::BTreeMap;

use chrono::{DateTime, LocalResult};
use datalib_time::NamedZone;
use serde_json::Value;

/// What a clock change moves by when a time zone moved it.
const ZONE_STEP_S: i64 = 900;

/// How far a logged jump can be from a whole [`ZONE_STEP_S`]: the device
/// logs the two sides a few seconds apart.
const JUMP_SLACK_S: i64 = 60;

/// How far ahead of now a reading can be before it is impossible rather
/// than two clocks disagreeing a little.
const AHEAD_LIMIT_MS: i64 = 15 * 60 * 1000;

/// One line of `history.txt`: the clock jumped, and the files from before
/// it went into `archive<archive>/`. Both times are as the device counts
/// them, which is its local clock read as UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockChange {
    pub archive: i64,
    pub previous_wall_s: i64,
    pub after_wall_s: i64,
    pub after_timezone: Option<String>,
}

/// The changes `history.txt` holds, and a sentence for each line it could
/// not read.
pub fn parse_history(text: &str) -> (Vec<ClockChange>, Vec<String>) {
    let mut changes = Vec::new();
    let mut bad = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_change(line) {
            Some(change) => changes.push(change),
            None => bad.push(format!("line {} of history.txt will not read", n + 1)),
        }
    }
    changes.sort_by_key(|c| c.archive);
    (changes, bad)
}

fn parse_change(line: &str) -> Option<ClockChange> {
    let v: Value = serde_json::from_str(line).ok()?;
    let wall = |key: &str| {
        v[key]
            .as_i64()
            .or_else(|| v[key].as_str().and_then(|s| s.parse().ok()))
    };
    Some(ClockChange {
        archive: wall("archive")?,
        previous_wall_s: wall("previous_t")?,
        after_wall_s: wall("after_t")?,
        after_timezone: v["after_timezone"].as_str().map(str::to_string),
    })
}

/// The offset of the stretch since a device's last clock change.
#[derive(Debug, Clone)]
enum Latest {
    Fixed(i32),
    /// No change is on record, so nothing says when it last moved.
    Zone(NamedZone),
    Unknown,
}

/// Each stretch between two clock changes has one offset. A device applies
/// a time-zone rule late, by hours or a day, so the stretch it was in says
/// what its clock meant and the zone's rule for that moment does not.
#[derive(Debug, Clone)]
pub struct ClockModel {
    archives: BTreeMap<i64, i32>,
    latest: Latest,
}

fn local_offset_s(zone: &NamedZone, wall_s: i64, went_back: bool) -> Option<i32> {
    let local = DateTime::from_timestamp(wall_s, 0)?.naive_utc();
    let at = match zone.from_local(&local) {
        LocalResult::Single(at) => at,
        // The clocks going back: a change landing here is on its second pass.
        LocalResult::Ambiguous(earlier, later) => {
            if went_back {
                later
            } else {
                earlier
            }
        }
        LocalResult::None => return None,
    };
    Some(at.offset().local_minus_utc())
}

/// The model for a device's changes, and a sentence for each thing that
/// makes it doubtful. `timezone` is what the device says now, for one with
/// no change on record.
pub fn clock_model(changes: &[ClockChange], timezone: Option<&str>) -> (ClockModel, Vec<String>) {
    let mut archives = BTreeMap::new();
    let mut trouble = Vec::new();
    let mut after: Option<i32> = None;
    for change in changes {
        let jump = change.after_wall_s - change.previous_wall_s;
        let step = (jump as f64 / ZONE_STEP_S as f64).round() as i64 * ZONE_STEP_S;
        if (jump - step).abs() > JUMP_SLACK_S {
            trouble.push(format!(
                "the clock jumped {jump}s at archive {}, which no time zone explains",
                change.archive
            ));
        }
        let zone = change.after_timezone.as_deref().and_then(NamedZone::get);
        if zone.is_none() {
            trouble.push(format!(
                "archive {} names a time zone this build does not know: {}",
                change.archive,
                change.after_timezone.as_deref().unwrap_or("(none)")
            ));
        }
        let now_at = zone.and_then(|z| local_offset_s(&z, change.after_wall_s, jump < 0));
        let before = now_at.map(|o| o - step as i32).or(after);
        if let (Some(were), Some(before)) = (after, before) {
            if were != before {
                trouble.push(format!(
                    "archive {} starts at UTC{:+}s but the change before it left the clock at \
                     UTC{:+}s",
                    change.archive, before, were
                ));
            }
        }
        if let Some(before) = before {
            archives.insert(change.archive, before);
        }
        after = now_at;
    }
    let latest = match (changes.is_empty(), after) {
        (false, Some(offset)) => Latest::Fixed(offset),
        (false, None) => Latest::Unknown,
        (true, _) => match timezone.and_then(NamedZone::get) {
            Some(zone) => Latest::Zone(zone),
            None => {
                trouble.push(format!(
                    "no time zone is known ({}), so its times are as the device wrote them",
                    timezone.unwrap_or("the device does not say")
                ));
                Latest::Unknown
            }
        },
    };
    (ClockModel { archives, latest }, trouble)
}

fn archive_of(source_file: &str) -> Option<i64> {
    source_file
        .split('/')
        .next()?
        .strip_prefix("archive")?
        .parse()
        .ok()
}

impl ClockModel {
    fn offset_s(&self, source_file: &str, wall_s: i64) -> Option<i32> {
        match archive_of(source_file) {
            Some(archive) => self.archives.get(&archive).copied(),
            None => match &self.latest {
                Latest::Fixed(offset) => Some(*offset),
                Latest::Zone(zone) => local_offset_s(zone, wall_s, false),
                Latest::Unknown => None,
            },
        }
    }

    /// The real time of a reading the device stamped `wall_ms`, in the file
    /// `source_file` (relative to the device's folder), or `wall_ms` as it
    /// stands where [`clock_model`] reported it could not say.
    pub fn real_ms(&self, source_file: &str, wall_ms: i64) -> i64 {
        match self.offset_s(source_file, wall_ms.div_euclid(1000)) {
            Some(offset) => wall_ms - i64::from(offset) * 1000,
            None => wall_ms,
        }
    }

    /// A sentence for each `archive<N>/` folder among `files` that
    /// `history.txt` has no change for, so no offset.
    pub fn unplaced_archives<'a>(&self, files: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut archives: Vec<i64> = files
            .filter_map(archive_of)
            .filter(|a| !self.archives.contains_key(a))
            .collect();
        archives.sort_unstable();
        archives.dedup();
        archives
            .into_iter()
            .map(|a| format!("archive{a} has no line in history.txt"))
            .collect()
    }
}

/// A sentence when a reading's real time is impossible: still in the future.
pub fn ahead_of_now(real_ms: i64, now_ms: i64) -> Option<String> {
    let ahead = real_ms - now_ms;
    (ahead > AHEAD_LIMIT_MS).then(|| {
        format!(
            "its newest reading is {} minutes in the future, so its clock or time zone is wrong",
            ahead / 60_000
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    fn change(archive: i64, previous: &str, after: &str, zone: &str) -> ClockChange {
        let wall = |s: &str| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_utc()
                .timestamp()
        };
        ClockChange {
            archive,
            previous_wall_s: wall(previous),
            after_wall_s: wall(after),
            after_timezone: Some(zone.to_string()),
        }
    }

    /// A voyage: autumn and spring in San Francisco, a move across nine
    /// hours, then autumn in Paris applied late.
    fn voyage() -> Vec<ClockChange> {
        vec![
            change(
                1,
                "2023-11-05 03:12:20",
                "2023-11-05 02:12:18",
                "America/Los_Angeles",
            ),
            change(
                2,
                "2024-03-10 02:14:30",
                "2024-03-10 03:14:28",
                "America/Los_Angeles",
            ),
            change(
                3,
                "2024-06-18 14:05:40",
                "2024-06-18 23:05:40",
                "Europe/Paris",
            ),
            change(
                4,
                "2024-10-27 06:22:51",
                "2024-10-27 05:22:48",
                "Europe/Paris",
            ),
        ]
    }

    /// The whole point: real time runs on through every jump the "epoch"
    /// makes, by no more than the seconds the device took to log it.
    #[test]
    fn real_time_runs_straight_through_every_clock_change() {
        let changes = voyage();
        let (model, trouble) = clock_model(&changes, None);
        assert_eq!(trouble, Vec::<String>::new());
        for (i, c) in changes.iter().enumerate() {
            let next = changes.get(i + 1).map_or("x.txt".to_string(), |n| {
                format!("archive{}/x.txt", n.archive)
            });
            let before = model.real_ms(
                &format!("archive{}/x.txt", c.archive),
                c.previous_wall_s * 1000,
            );
            let after = model.real_ms(&next, c.after_wall_s * 1000);
            assert!(
                (after - before).abs() < 5_000,
                "archive {}: {}",
                c.archive,
                after - before
            );
        }
    }

    #[test]
    fn a_reading_is_converted_by_the_stretch_it_is_in() {
        let (model, _) = clock_model(&voyage(), None);
        let wall = 1_780_000_000_i64;
        assert_eq!(
            model.real_ms("archive1/x.txt", wall * 1000),
            (wall + 7 * HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("archive3/x.txt", wall * 1000),
            (wall + 7 * HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("archive2/x.txt", wall * 1000),
            (wall + 8 * HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("archive4/x.txt", wall * 1000),
            (wall - 2 * HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("202611_x.txt", wall * 1000),
            (wall - HOUR) * 1000
        );
    }

    /// The device applies the rule late. At 06:06 on the morning the Paris
    /// clocks went back, the zone says UTC+1, but this device had not yet
    /// moved, and its clock still meant UTC+2.
    #[test]
    fn a_change_the_device_applied_late_belongs_to_the_stretch_it_was_in() {
        let (model, _) = clock_model(&voyage(), None);
        let wall = voyage()[3].previous_wall_s - 16 * 60;
        assert_eq!(
            model.real_ms("archive4/x.txt", wall * 1000),
            (wall - 2 * HOUR) * 1000
        );
    }

    /// Landing in the repeated hour is the second pass through it.
    #[test]
    fn a_change_landing_in_the_repeated_hour_is_after_the_clocks_went_back() {
        let changes = [change(
            1,
            "2022-11-06 02:09:33",
            "2022-11-06 01:09:31",
            "America/Los_Angeles",
        )];
        let (model, trouble) = clock_model(&changes, None);
        assert_eq!(trouble, Vec::<String>::new());
        let wall = changes[0].after_wall_s;
        assert_eq!(
            model.real_ms("x.txt", wall * 1000),
            (wall + 8 * HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("archive1/x.txt", wall * 1000),
            (wall + 7 * HOUR) * 1000
        );
    }

    #[test]
    fn with_no_change_on_record_the_zone_says() {
        let (model, trouble) = clock_model(&[], Some("Europe/Paris"));
        assert_eq!(trouble, Vec::<String>::new());
        let winter = 1_767_225_600_i64;
        let summer = 1_782_864_000_i64;
        assert_eq!(
            model.real_ms("x.txt", winter * 1000),
            (winter - HOUR) * 1000
        );
        assert_eq!(
            model.real_ms("x.txt", summer * 1000),
            (summer - 2 * HOUR) * 1000
        );
    }

    /// What cannot be converted stays as written, and says so.
    #[test]
    fn a_clock_that_cannot_be_trusted_is_reported() {
        let wall = 1_780_000_000_000;
        let (model, trouble) = clock_model(&[], None);
        assert_eq!(model.real_ms("x.txt", wall), wall);
        assert!(trouble[0].contains("no time zone is known"), "{trouble:?}");

        let (model, trouble) = clock_model(&[], Some("Earth/San_Francisco"));
        assert_eq!(model.real_ms("x.txt", wall), wall);
        assert!(trouble[0].contains("Earth/San_Francisco"), "{trouble:?}");

        let (_, trouble) = clock_model(
            &[change(
                1,
                "2024-06-18 14:05:40",
                "2024-06-18 14:10:40",
                "Europe/Paris",
            )],
            None,
        );
        assert!(trouble[0].contains("no time zone explains"), "{trouble:?}");

        let mut changes = voyage();
        changes[3].after_timezone = Some("America/Los_Angeles".to_string());
        let (_, trouble) = clock_model(&changes, None);
        assert!(trouble[0].contains("archive 4 starts at"), "{trouble:?}");

        let (_, trouble) = clock_model(
            &[change(
                1,
                "2024-06-18 14:05:40",
                "2024-06-18 15:05:40",
                "Earth/Vulcan",
            )],
            None,
        );
        assert!(trouble[0].contains("Earth/Vulcan"), "{trouble:?}");
    }

    #[test]
    fn an_archive_history_does_not_name_is_reported() {
        let (model, _) = clock_model(&voyage(), None);
        let found = model.unplaced_archives(
            [
                "archive1/a.txt",
                "archive9/b.txt",
                "archive9/c.txt",
                "d.txt",
            ]
            .into_iter(),
        );
        assert_eq!(found, ["archive9 has no line in history.txt"]);
    }

    /// Guards the bug this file exists for: every reading an hour in the
    /// future for years, and nothing said so.
    #[test]
    fn a_reading_in_the_future_is_impossible() {
        let now = 1_730_000_000_000;
        assert_eq!(ahead_of_now(now, now), None);
        assert_eq!(ahead_of_now(now + 14 * 60_000, now), None);
        assert_eq!(
            ahead_of_now(now + 56 * 60_000, now).as_deref(),
            Some("its newest reading is 56 minutes in the future, so its clock or time zone is wrong")
        );
    }

    #[test]
    fn a_line_that_will_not_read_costs_that_line() {
        let text = "{\"archive\":2,\"previous_t\":\"100\",\"after_t\":\"3700\",\"after_timezone\":\"Europe/Paris\"}\n\nnot json\n{\"archive\":1,\"previous_t\":5,\"after_t\":6}\n";
        let (changes, bad) = parse_history(text);
        assert_eq!(bad, ["line 3 of history.txt will not read"]);
        assert_eq!(
            changes.iter().map(|c| c.archive).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(changes[1].after_wall_s, 3700);
        assert_eq!(changes[0].after_timezone, None);
    }
}
