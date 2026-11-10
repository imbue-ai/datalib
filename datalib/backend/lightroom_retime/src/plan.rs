//! What to write for one edit, decided from the photo's rows as read:
//! the three places Lightroom keeps a capture time, each feeding one thing
//! it does (lightroom/tests/capture_time/RESULTS.md §"Round 5").
//!
//! - `Adobe_images.captureTime`, the clock time with no offset: the sort.
//! - the XMP's `exif:DateTimeOriginal` and `photoshop:DateCreated`, with
//!   the offset: what the Metadata panel shows, and what Edit Capture Time
//!   shifts from.
//! - `AgHarvestedExifMetadata.dateYear/Month/Day`: the day the Library
//!   filter files the photo under.

use chrono::{DateTime, Datelike, FixedOffset, Timelike};

use crate::edits::Edit;
use crate::xmp;

/// One photo's rows, as the catalog holds them.
#[derive(Debug, Clone, PartialEq)]
pub struct Photo {
    pub image: i64,
    pub name: String,
    pub capture_time: Option<String>,
    pub original_capture_time: Option<String>,
    pub xmp: Option<Vec<u8>>,
    pub exif_rows: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Write {
    pub image: i64,
    pub capture_time: String,
    /// Set only where Lightroom has not kept a first time already.
    pub original_capture_time: Option<String>,
    pub xmp: Vec<u8>,
    pub year: f64,
    pub month: f64,
    pub day: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Write(Write),
    AlreadyDone,
    Refused(String),
}

const XMP_DATES: [xmp::Prop; 2] = [xmp::DATE_TIME_ORIGINAL, xmp::DATE_CREATED];

/// `captureTime` as Lightroom writes it: the clock time to the millisecond.
pub fn catalog_clock(t: &DateTime<FixedOffset>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()
}

pub fn xmp_time(t: &DateTime<FixedOffset>) -> String {
    if t.nanosecond() == 0 {
        t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
    } else {
        t.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
    }
}

pub fn plan(edit: &Edit, rows: &[Photo]) -> Outcome {
    let photo = match rows {
        [one] => one,
        [] => return Outcome::Refused("no photo has this id_global".into()),
        _ => return Outcome::Refused(format!("{} photos have this id_global", rows.len())),
    };
    let clock = catalog_clock(&edit.target);
    let stamp = xmp_time(&edit.target);
    let Some(blob) = photo.xmp.as_deref().filter(|b| !b.is_empty()) else {
        return Outcome::Refused("the photo has no XMP in the catalog".into());
    };
    let text = match xmp::unpack(blob) {
        Ok(t) => t,
        Err(e) => return Outcome::Refused(e),
    };
    let current = photo.capture_time.as_deref();
    if current != Some(edit.expected.as_str()) {
        if current == Some(clock.as_str()) && already_set(&text, &stamp) {
            return Outcome::AlreadyDone;
        }
        return Outcome::Refused(format!(
            "captureTime is {current:?}, not the expected {:?}",
            edit.expected
        ));
    }
    if photo.exif_rows == 0 {
        return Outcome::Refused("the photo has no AgHarvestedExifMetadata row".into());
    }
    let new_xmp = match xmp::set(&text, &XMP_DATES, &stamp) {
        Ok(t) => t,
        Err(e) => return Outcome::Refused(e),
    };
    let original_kept = photo
        .original_capture_time
        .as_deref()
        .is_some_and(|o| !o.is_empty());
    let date = edit.target.date_naive();
    Outcome::Write(Write {
        image: photo.image,
        capture_time: clock,
        original_capture_time: (!original_kept).then(|| edit.expected.clone()),
        xmp: xmp::pack(&new_xmp),
        year: f64::from(date.year()),
        month: f64::from(date.month()),
        day: f64::from(date.day()),
    })
}

fn already_set(text: &str, stamp: &str) -> bool {
    XMP_DATES
        .iter()
        .all(|&p| xmp::get(text, p).ok().flatten().as_deref() == Some(stamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XMP: &str = concat!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
        "<rdf:Description rdf:about=\"\" xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"",
        " exif:DateTimeOriginal=\"2024-08-01T21:00:00\"/>",
        "</rdf:RDF></x:xmpmeta>"
    );

    fn photo() -> Photo {
        Photo {
            image: 7,
            name: "F10.jpg".into(),
            capture_time: Some("2024-08-01T21:00:00.000".into()),
            original_capture_time: None,
            xmp: Some(xmp::pack(XMP)),
            exif_rows: 1,
        }
    }

    fn edit(capture_time: &str) -> Edit {
        Edit {
            line: 1,
            id_global: "ID".into(),
            expected: "2024-08-01T21:00:00.000".into(),
            target: DateTime::parse_from_rfc3339(capture_time).unwrap(),
            why: None,
        }
    }

    fn written(o: Outcome) -> Write {
        match o {
            Outcome::Write(w) => w,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    /// 06:00 +09:00 on Aug 2 is Aug 1 in UTC; Lightroom's date filter must
    /// file it under the local day, as F10 showed.
    #[test]
    fn the_day_columns_take_the_local_date() {
        let w = written(plan(&edit("2024-08-02T06:00:00+09:00"), &[photo()]));
        assert_eq!((w.year, w.month, w.day), (2024.0, 8.0, 2.0));
        assert_eq!(w.capture_time, "2024-08-02T06:00:00.000");
        let text = xmp::unpack(&w.xmp).unwrap();
        assert_eq!(
            xmp::get(&text, xmp::DATE_CREATED).unwrap().as_deref(),
            Some("2024-08-02T06:00:00+09:00")
        );
    }

    #[test]
    fn the_first_time_is_kept_only_where_lightroom_has_none() {
        let w = written(plan(&edit("2024-08-02T06:00:00+09:00"), &[photo()]));
        assert_eq!(
            w.original_capture_time.as_deref(),
            Some("2024-08-01T21:00:00.000")
        );
        let mut edited = photo();
        edited.original_capture_time = Some("2024-08-01T20:00:00.000".into());
        let w = written(plan(&edit("2024-08-02T06:00:00+09:00"), &[edited]));
        assert_eq!(w.original_capture_time, None);
    }

    #[test]
    fn a_photo_changed_since_the_edits_were_made_is_refused() {
        let mut moved = photo();
        moved.capture_time = Some("2024-08-01T22:00:00.000".into());
        let Outcome::Refused(why) = plan(&edit("2024-08-02T06:00:00+09:00"), &[moved]) else {
            panic!("not refused");
        };
        assert!(why.contains("not the expected"), "{why}");
    }

    #[test]
    fn an_edit_already_applied_is_left_alone() {
        let e = edit("2024-08-02T06:00:00+09:00");
        let w = written(plan(&e, &[photo()]));
        let mut done = photo();
        done.capture_time = Some(w.capture_time);
        done.xmp = Some(w.xmp);
        assert_eq!(plan(&e, &[done]), Outcome::AlreadyDone);
    }

    #[test]
    fn subseconds_are_kept() {
        let w = written(plan(&edit("2024-08-02T06:00:00.250+09:00"), &[photo()]));
        assert_eq!(w.capture_time, "2024-08-02T06:00:00.250");
        let text = xmp::unpack(&w.xmp).unwrap();
        assert_eq!(
            xmp::get(&text, xmp::DATE_TIME_ORIGINAL).unwrap().as_deref(),
            Some("2024-08-02T06:00:00.250+09:00")
        );
    }

    #[test]
    fn a_missing_or_doubled_photo_is_refused() {
        let e = edit("2024-08-02T06:00:00+09:00");
        assert!(matches!(plan(&e, &[]), Outcome::Refused(_)));
        assert!(matches!(plan(&e, &[photo(), photo()]), Outcome::Refused(_)));
        let mut no_exif = photo();
        no_exif.exif_rows = 0;
        assert!(matches!(plan(&e, &[no_exif]), Outcome::Refused(_)));
    }
}
