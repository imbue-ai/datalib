//! One catalog image as a photo: its `NormalizedPhoto`, its grid row and
//! search terms, and what the render could not know. Pure; the processor
//! reads the rows and emits what this returns.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use datalib_etl_render::section::Section;
use datalib_id::{IdNamespace, Identity};
use datalib_photo_schema::{
    Exposure, Geo, H3Cell, MediaKind, NormalizedPhoto, OffsetFrom, Original, Pictures,
};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Scope, Severity, Stage};
use datalib_schema::providers::Provider;
use datalib_schema::search_terms::{SearchTerm, SearchTermKind};
use datalib_time::{parse_strict, NamedZone};

use crate::read::ImageRow;
use crate::xmp::{self, XmpFields};

/// Bump when what a photo's row or terms say changes.
pub const RENDER_VERSION: u32 = 3;

/// The H3 cells a photo's card names: about a region, a town, a
/// neighbourhood and a block. Search does not need them; it reads the
/// row's location (`grid_rows.h3`).
pub const H3_RESOLUTIONS: [u8; 4] = [4, 6, 8, 10];

/// Seconds from the unix epoch to 2001-01-01, where Lightroom counts from.
const APPLE_EPOCH: i64 = 978_307_200;

pub const ENTITY_KIND: &str = "image";

pub struct Built {
    pub photo: NormalizedPhoto,
    pub row: Option<GridRow>,
    pub section: Section,
    pub problems: Vec<ProblemRow>,
}

pub struct Context<'a> {
    pub source_id: &'a str,
    /// The data-root-relative path every document of this source files
    /// under, from `doc_file_rel`.
    pub qmd_path: &'a str,
    pub catalog: Option<&'a str>,
    pub zone: Option<&'a NamedZone>,
}

pub fn uuid(source_id: &str, id_global: &str) -> String {
    Identity::mint(
        IdNamespace::Lightroom,
        source_id,
        None,
        ENTITY_KIND,
        id_global.to_string(),
        None,
    )
    .uuid
}

pub fn build(image: &ImageRow, ctx: &Context<'_>) -> Built {
    let uuid = uuid(ctx.source_id, &image.id_global);
    let xmp = image
        .xmp
        .as_deref()
        .and_then(xmp::decompress)
        .map(|p| xmp::fields(&p))
        .unwrap_or_default();
    let capture = image
        .capture_time
        .as_deref()
        .and_then(|t| capture_time(t, &xmp, ctx.zone));
    let mut problems = Vec::new();
    if let Some(Capture {
        from: OffsetFrom::AssumedUtc,
        ..
    }) = &capture
    {
        problems.push(assumed_utc(ctx.source_id, &uuid));
    }
    let geo = image
        .gps
        .filter(|&(lat, lng)| datalib_h3::locate(lat, lng).is_some())
        .map(|(latitude, longitude)| Geo {
            latitude,
            longitude,
        });
    let file_name = match &image.extension {
        Some(ext) => format!("{}.{ext}", image.base_name),
        None => image.base_name.clone(),
    };
    let in_folder =
        |root: &str| format!("{root}{}{file_name}", image.folder.as_deref().unwrap_or(""));
    let photo = NormalizedPhoto {
        uuid: uuid.clone(),
        source_id: ctx.source_id.to_string(),
        kind: if image.file_format.as_deref() == Some("VIDEO") {
            MediaKind::Video
        } else {
            MediaKind::Photo
        },
        title: xmp.title.clone(),
        headline: xmp.headline.clone(),
        caption: image.caption.clone(),
        copyright: image.copyright.clone(),
        creator: image.creator.clone(),
        captured_at: capture.as_ref().map(|c| c.at.clone()),
        offset_from: capture.as_ref().map(|c| c.from),
        edited_at_utc: image.touch_time.and_then(apple_seconds),
        camera: image.camera.clone(),
        lens: image.lens.clone(),
        exposure: Exposure {
            iso: image.iso,
            shutter_seconds: image.shutter_apex.map(|a| round(2f64.powf(-a), 6)),
            aperture: image.aperture_apex.map(|a| round(2f64.powf(a / 2.0), 1)),
            focal_length_mm: image.focal_length,
        },
        rating: image.rating.filter(|r| *r > 0.0).map(|r| r as u8),
        pick: image.pick.map(|p| p as i8),
        color_label: image.color_label.clone(),
        keywords: image.keywords.clone(),
        people: image.people.clone(),
        albums: image.albums.clone(),
        place: image.place.clone(),
        geo,
        h3: geo
            .map(|g| h3_cells(g.latitude, g.longitude))
            .unwrap_or_default(),
        width: image.width.map(|w| w as u32),
        height: image.height.map(|h| h as u32),
        duration_seconds: image.duration.as_deref().and_then(|d| d.parse().ok()),
        file_name: match &image.copy_name {
            Some(copy) => format!("{file_name} ({copy})"),
            None => file_name.clone(),
        },
        folder: image.folder.clone(),
        original: Original {
            absolute: image.root_absolute.as_deref().map(in_folder),
            relative_to_catalog: image.root_relative.as_deref().map(in_folder),
        },
        pictures: Pictures::Lightroom {
            catalog: ctx.catalog.map(str::to_string),
            image_id_local: image.id_local,
            file_uuid: image.file_uuid.clone(),
        },
    };
    let row = grid_row(&photo, image, ctx, &mut problems);
    let section = Section::keyed(&uuid, summary(&photo));
    Built {
        photo,
        row,
        section,
        problems,
    }
}

fn grid_row(
    photo: &NormalizedPhoto,
    image: &ImageRow,
    ctx: &Context<'_>,
    problems: &mut Vec<ProblemRow>,
) -> Option<GridRow> {
    let subject = photo
        .title
        .clone()
        .or_else(|| photo.headline.clone())
        .unwrap_or_else(|| photo.file_name.clone());
    let mut row = GridRow::builder()
        .uuid(photo.uuid.clone())
        .provider(Provider::Lightroom)
        .kind(match photo.kind {
            MediaKind::Photo => "Photo",
            MediaKind::Video => "Video",
        })
        .source_label("Lightroom")
        .is_document(true)
        .item_count(Some(1))
        .created_at(photo.captured_at.clone())
        .modified_at(photo.edited_at_utc.clone())
        // A grid of photos newest first means newest taken, not edited.
        .touched_at(photo.captured_at.clone().or(photo.edited_at_utc.clone()))
        .author(photo.creator.clone())
        .channel(photo.folder.clone())
        .conversation_name(Some(subject))
        .conversation_uuid(photo.uuid.clone())
        .entire_chat(format!("/chat/{}", photo.uuid))
        .body(photo.caption.clone().unwrap_or_default())
        .qmd_path(Some(ctx.qmd_path.to_string()))
        .markdown_uuid(Some(photo.uuid.clone()))
        .upstream_id(Some(image.id_global.clone()))
        .upstream_entity_kind(Some(ENTITY_KIND.to_string()))
        .image(Some(datalib_uri::Name::photo(&photo.uuid).write()))
        .geo(photo.geo.map(|g| (g.latitude, g.longitude)))
        .build_or_record(ctx.source_id, &photo.uuid, RENDER_VERSION, problems)?;
    row.set_supplied_terms(terms(photo, image));
    Some(row)
}

/// What a photo answers to in search: its keywords and every keyword
/// above them, its people and albums as labels; its place names; its
/// caption, title and Lightroom's own id. Its location is the row's own
/// column, which `place:<cell>` reads.
pub fn terms(photo: &NormalizedPhoto, image: &ImageRow) -> Vec<SearchTerm> {
    let label = |v: &str| SearchTerm::new(SearchTermKind::Label, v);
    let place = |v: &str| SearchTerm::new(SearchTermKind::Place, v);
    let mut out: Vec<SearchTerm> = Vec::new();
    out.extend(photo.keywords.iter().flatten().map(|k| label(k)));
    out.extend(photo.people.iter().map(|p| label(&p.name)));
    out.extend(photo.albums.iter().map(|a| label(a)));
    out.extend(photo.place.names().map(place));
    out.extend(
        photo
            .caption
            .iter()
            .map(|c| SearchTerm::new(SearchTermKind::Caption, c)),
    );
    out.extend(
        [&photo.title, &photo.headline]
            .into_iter()
            .flatten()
            .map(|t| SearchTerm::new(SearchTermKind::Title, t)),
    );
    out.push(SearchTerm::new(
        SearchTermKind::UpstreamId,
        &image.id_global,
    ));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub at: String,
    pub from: OffsetFrom,
}

/// When a photo was taken, with an offset: Lightroom's wall clock, which
/// the person may have edited, and the offset the photo's own metadata
/// recorded, else the configured zone's, else UTC.
pub fn capture_time(capture: &str, xmp: &XmpFields, zone: Option<&NamedZone>) -> Option<Capture> {
    if parse_strict(capture).is_ok() {
        return Some(Capture {
            at: capture.to_string(),
            from: OffsetFrom::DateTimeOriginal,
        });
    }
    let wall = civil(capture)?;
    let recorded = [
        (&xmp.date_time_original, OffsetFrom::DateTimeOriginal),
        (&xmp.create_date, OffsetFrom::CreateDate),
    ]
    .into_iter()
    .find_map(|(stamp, from)| stamp.as_deref().and_then(offset_of).map(|o| (o, from)));
    let (offset, from) = match (recorded, zone) {
        (Some(found), _) => found,
        (None, Some(zone)) => (zone_offset(zone, &wall), OffsetFrom::ConfiguredZone),
        (None, None) => ("+00:00".to_string(), OffsetFrom::AssumedUtc),
    };
    let at = format!("{capture}{offset}");
    parse_strict(&at).is_ok().then_some(Capture { at, from })
}

fn civil(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

/// The `+hh:mm` a stamp ends in; `Z` is `+00:00`.
fn offset_of(stamp: &str) -> Option<String> {
    if stamp.ends_with('Z') {
        return Some("+00:00".to_string());
    }
    let tail = stamp.get(stamp.len().checked_sub(6)?..)?;
    let b = tail.as_bytes();
    let shaped = (b[0] == b'+' || b[0] == b'-')
        && b[3] == b':'
        && [1, 2, 4, 5].iter().all(|&i| b[i].is_ascii_digit());
    shaped.then(|| tail.to_string())
}

/// The zone's offset at a wall-clock time: the earlier one in the hour
/// the clocks go back, and in the hour they skip, the one in force just
/// before it.
fn zone_offset(zone: &NamedZone, wall: &NaiveDateTime) -> String {
    use chrono::LocalResult;
    let at = match zone.from_local(wall) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t,
        LocalResult::None => zone.at(&Utc.from_utc_datetime(wall)),
    };
    at.format("%:z").to_string()
}

fn apple_seconds(seconds: f64) -> Option<String> {
    let unix = APPLE_EPOCH + seconds.floor() as i64;
    DateTime::<Utc>::from_timestamp(unix, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

/// The cells a fix is in, each the parent of its finest cell, so that
/// searching any of them finds the photo (datalib/backend/h3/README.md).
pub fn h3_cells(latitude: f64, longitude: f64) -> Vec<H3Cell> {
    let Some(finest) = datalib_h3::locate(latitude, longitude) else {
        return Vec::new();
    };
    H3_RESOLUTIONS
        .iter()
        .filter_map(|&r| {
            let resolution = datalib_h3::Resolution::try_from(r).ok()?;
            Some(H3Cell {
                resolution: r,
                cell: datalib_h3::ancestor(finest, resolution)?.to_string(),
            })
        })
        .collect()
}

fn round(v: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (v * scale).round() / scale
}

fn assumed_utc(source_id: &str, uuid: &str) -> ProblemRow {
    ProblemRow::new(
        source_id,
        Stage::Render,
        Scope::Markdown(uuid),
        Some(uuid),
        Outcome::Nulled,
        Problem::lossy_explained(
            "capture_time_assumed_utc",
            Some("created_at".to_string()),
            "the photo records no time zone and the source names none \
             (`capture_time_zone`), so its time is read as UTC",
        )
        .severity(Severity::Warning),
        Some(RENDER_VERSION),
    )
}

/// The photo as text, for a diff of two renders: there is no page on
/// disk, and this is what one would say.
fn summary(photo: &NormalizedPhoto) -> String {
    let mut out = format!(
        "# {}\n\n",
        photo
            .title
            .as_deref()
            .or(photo.headline.as_deref())
            .unwrap_or(&photo.file_name)
    );
    if let Some(caption) = &photo.caption {
        out.push_str(&format!("{caption}\n\n"));
    }
    let mut line = |label: &str, value: String| {
        if !value.is_empty() {
            out.push_str(&format!("- {label}: {value}\n"));
        }
    };
    line("File", photo.file_name.clone());
    line("Taken", photo.captured_at.clone().unwrap_or_default());
    line(
        "Keywords",
        photo
            .keywords
            .iter()
            .map(|k| k.join(" > "))
            .collect::<Vec<_>>()
            .join(", "),
    );
    line(
        "People",
        photo
            .people
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
    line("Albums", photo.albums.join(", "));
    line("Place", photo.place.names().collect::<Vec<_>>().join(", "));
    line(
        "Rating",
        photo
            .rating
            .map(|r| "★".repeat(r as usize))
            .unwrap_or_default(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xmp(original: Option<&str>, create: Option<&str>) -> XmpFields {
        XmpFields {
            date_time_original: original.map(str::to_string),
            create_date: create.map(str::to_string),
            ..XmpFields::default()
        }
    }

    /// The photo's own offset wins over the configured zone, the wall
    /// clock stays Lightroom's (which the person may have edited), and a
    /// photo with neither is read as UTC and says so.
    #[test]
    fn the_offset_comes_from_the_photo_then_the_config_then_utc() {
        let berlin = NamedZone::get("Europe/Berlin").unwrap();
        let wall = "2364-03-02T07:59:12";
        let recorded = xmp(
            Some("2364-03-02T07:59:00-08:00"),
            Some("2364-03-02T09:00:00+01:00"),
        );
        assert_eq!(
            capture_time(wall, &recorded, Some(&berlin)),
            Some(Capture {
                at: "2364-03-02T07:59:12-08:00".into(),
                from: OffsetFrom::DateTimeOriginal
            })
        );
        let only_create = xmp(
            Some("2364-03-02T07:59:12.72"),
            Some("2364-03-02T07:59:12+01:00"),
        );
        assert_eq!(
            capture_time("2364-03-02T07:59:12.72", &only_create, None).map(|c| c.from),
            Some(OffsetFrom::CreateDate)
        );
        assert_eq!(
            capture_time("2025-07-01T12:00:00", &XmpFields::default(), Some(&berlin)),
            Some(Capture {
                at: "2025-07-01T12:00:00+02:00".into(),
                from: OffsetFrom::ConfiguredZone
            })
        );
        assert_eq!(
            capture_time(wall, &XmpFields::default(), None),
            Some(Capture {
                at: "2364-03-02T07:59:12+00:00".into(),
                from: OffsetFrom::AssumedUtc
            })
        );
        assert_eq!(
            capture_time("not a time", &XmpFields::default(), None),
            None
        );
    }

    /// The warning on a photo read as UTC says so: cut at 80 characters
    /// it ended before "read as UTC", and so never said what was done.
    #[test]
    fn the_assumed_utc_warning_is_kept_whole() {
        let row = assumed_utc("lightroom", "00000000-0000-8000-8000-000000000001");
        assert!(
            row.sample.ends_with("so its time is read as UTC"),
            "{}",
            row.sample
        );
        assert_eq!(row.rule.as_deref(), Some("capture_time_assumed_utc"));
    }

    #[test]
    fn touch_time_counts_from_2001() {
        assert_eq!(
            apple_seconds(0.5).as_deref(),
            Some("2001-01-01T00:00:00+00:00")
        );
    }

    /// Each cell the card names holds the photo's location by range, so
    /// following it finds the photo, even where locating the point at that
    /// resolution would name a neighbour.
    #[test]
    fn every_cell_named_holds_the_photos_location() {
        let finest = datalib_h3::locate(37.8275, -122.4819).unwrap();
        let cells = h3_cells(37.8275, -122.4819);
        assert_eq!(
            cells.iter().map(|c| c.resolution).collect::<Vec<_>>(),
            H3_RESOLUTIONS
        );
        for c in &cells {
            let cell = datalib_h3::parse(&c.cell).unwrap();
            assert!(datalib_h3::finest_range(cell).contains(&datalib_h3::stored(finest)));
        }
        assert!(h3_cells(91.0, 0.0).is_empty());
    }
}
