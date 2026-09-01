//! Hoisting the metadata worth querying into typed columns.
//!
//! Two readers, matching the two class tables:
//!
//! - **Audio** goes through `lofty`, which puts ID3v2, Vorbis comments,
//!   MP4 `ilst` atoms, RIFF `INFO` and APEv2 behind one interface and
//!   also reports bitrate, sample rate, channels and duration.
//! - **Visual** goes through `kamadak-exif` for the EXIF/TIFF IFD that
//!   JPEG, PNG, HEIF, WebP and DNG all embed, plus a small amount of
//!   direct container reading for the things EXIF does not carry:
//!   video duration, frame rate, codecs, and the dimensions of a file
//!   with no EXIF block at all.
//!
//! # Everything here is a hint
//!
//! No field in this module is trusted enough to key anything on. Tags
//! are typed by humans through a dozen tools with a dozen conventions,
//! and the failure modes are mundane rather than exotic: a compilation
//! where every track has a different `album_artist`, a camera whose
//! clock was in the wrong year, `artist` holding
//! `"Miles Davis feat. John Coltrane"` where a sibling file says
//! `"Miles Davis"`. We record what the file says and do not correct
//! it, for the same reason `pdf` stores producer junk in `author`
//! rather than filtering it: the heuristic that drops a routing code
//! eventually drops a real name.
//!
//! # Timestamps, and the one deviation from the repo convention
//!
//! AGENTS.md requires every stored timestamp to carry its source's UTC
//! offset. EXIF's `DateTimeOriginal` has none — it is local wall-clock
//! with no zone, and the offset only arrived with EXIF 2.31's
//! `OffsetTimeOriginal`, which most cameras still omit.
//!
//! So [`VisualMeta::captured_at`] carries an offset **when the file
//! supplies one** and is naive (`2026-05-04T03:42:05`) when it does
//! not. The alternatives were worse: stamping `+00:00` would assert
//! the photo was taken in UTC, and stamping the *scanning machine's*
//! offset would assert it was taken wherever the scan ran. A missing
//! offset is a fact about the file, and the naive form is the only
//! encoding that states it.
//!
//! A GPS-carrying photo could have its true offset recovered by
//! differencing `DateTimeOriginal` against the UTC `GPSDateStamp` /
//! `GPSTimeStamp`. That is a genuine follow-up, not a rejection.

use std::path::Path;

use anyhow::Result;

use super::kind::{Container, MediaClass};
use super::payload::{bmff, tiff, Src};

/// Tag- and property-derived fields for one audio item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    /// As tagged. Deliberately free-form text rather than a parsed
    /// date: real libraries hold `1997`, `1997-08`, `1997-08-25` and
    /// `08/25/1997`, and normalizing them all to one shape would throw
    /// away the distinction between "released in 1997" and "released on
    /// the 25th".
    pub date: Option<String>,
    pub track_no: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_no: Option<i64>,
    pub disc_total: Option<i64>,
    pub bitrate_kbps: Option<i64>,
    pub sample_rate_hz: Option<i64>,
    pub channels: Option<i64>,
    pub bit_depth: Option<i64>,
}

impl AudioMeta {
    /// Whether anything here was *typed by someone* rather than read
    /// off the stream. Bitrate and sample rate are properties of every
    /// audio track including the one inside a video, so they cannot be
    /// the test for "is this a tagged recording?".
    pub fn has_tags(&self) -> bool {
        self.title.is_some()
            || self.artist.is_some()
            || self.album.is_some()
            || self.album_artist.is_some()
            || self.composer.is_some()
            || self.genre.is_some()
            || self.date.is_some()
            || self.track_no.is_some()
            || self.disc_no.is_some()
    }
}

/// EXIF- and container-derived fields for one image or video item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VisualMeta {
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// EXIF orientation, 1–8. Stored raw rather than applied to
    /// `width`/`height`, so a consumer can tell "portrait" from
    /// "landscape shot with the camera turned".
    pub orientation: Option<i64>,
    /// See the module docs: offset-bearing when the file said so,
    /// naive when it did not.
    pub captured_at: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub iso: Option<i64>,
    /// As tagged, e.g. `1/250`. Kept as text for the same reason
    /// [`AudioMeta::date`] is.
    pub exposure_time: Option<String>,
    pub f_number: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_altitude_m: Option<f64>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub frame_rate: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

impl VisualMeta {
    /// Nothing was found. Emitting a row anyway would put an
    /// all-NULL `media_visual` entry behind every MP3.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Everything read from one file, plus the fields that belong on the
/// shared `media_items` row rather than a class table.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub audio: Option<AudioMeta>,
    pub visual: Option<VisualMeta>,
    pub duration_ms: Option<i64>,
    /// A short codec name for the item's principal stream.
    pub codec: Option<String>,
}

/// Read what this file will say about itself.
///
/// Both readers are attempted whenever the container could carry that
/// kind of metadata, rather than one being picked from the item's
/// class. The class is single-valued and a file need not be: an MP4
/// music video carries `ilst` tags *and* capture metadata, and an
/// `.m4a` shot on a phone carries a recording date in `©day`. Choosing
/// a reader by class silently dropped whichever half did not match.
///
/// A row is emitted for a class table only when there is something in
/// it — see [`AudioMeta::has_tags`] and [`VisualMeta::is_empty`] — so
/// this does not fill `media_visual` with a row per MP3.
///
/// Never fails the caller: a file whose tags are unreadable is still a
/// file we know exists, and the honest record of that is a row with
/// NULL columns. Parse failures are logged at `debug` and swallowed.
pub fn extract(path: &Path, class: MediaClass, container: Container) -> Meta {
    let mut meta = Meta::default();

    // ── Tags ─────────────────────────────────────────────────────────
    let mut tag_duration_ms = None;
    let mut tag_codec = None;
    if container.may_have_tags() {
        match read_audio(path) {
            Ok((audio, duration_ms, codec)) => {
                tag_duration_ms = duration_ms;
                tag_codec = codec;
                if class == MediaClass::Audio || audio.has_tags() {
                    // Properties alone (bitrate, sample rate) justify a
                    // row for an audio file and not for a video: every
                    // MP4 has an audio track, and a `media_audio` row
                    // per video would bury the music.
                    meta.audio = Some(audio);
                }
            }
            Err(e) => tracing::debug!(
                path = %path.display(), error = %e, "media_audio_meta_failed"
            ),
        }
    }

    // ── Capture ──────────────────────────────────────────────────────
    let mut visual = VisualMeta::default();
    if container.may_have_exif() {
        match read_exif(path) {
            Ok(v) => visual = v,
            Err(e) => tracing::debug!(
                path = %path.display(), error = %e, "media_exif_failed"
            ),
        }
    }
    if let Err(e) = read_structure(path, container, &mut visual, &mut meta) {
        tracing::debug!(path = %path.display(), error = %e, "media_structure_failed");
    }
    if class == MediaClass::Audio {
        // The stream codecs of an audio file belong on the item, not on
        // a capture row — and leaving them here would emit a
        // `media_visual` row for every tagged `.m4a`.
        tag_codec = tag_codec.or_else(|| visual.audio_codec.clone());
        visual.audio_codec = None;
        visual.video_codec = None;
    }
    if !visual.is_empty() {
        meta.visual = Some(visual.clone());
    }

    // ── The item's own columns ───────────────────────────────────────
    // The class decides which reader owns `duration_ms` and `codec`
    // when both have an opinion: the container walk knows a video's
    // timescale, the tag reader knows an audio file's.
    meta.duration_ms = match class {
        MediaClass::Audio => tag_duration_ms.or(meta.duration_ms),
        _ => meta.duration_ms.or(tag_duration_ms),
    };
    meta.codec = match class {
        MediaClass::Audio => tag_codec.or(meta.codec),
        _ => meta
            .codec
            .or_else(|| visual.video_codec.clone())
            .or(tag_codec),
    }
    .or_else(|| Some(container.as_str().to_string()));

    meta
}

// ─────────────────────────────────────────────────────────────────────
// Audio

fn read_audio(path: &Path) -> Result<(AudioMeta, Option<i64>, Option<String>)> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::prelude::{Accessor, ItemKey};

    let tagged = lofty::read_from_path(path)?;
    let props = tagged.properties();
    let duration_ms = i64::try_from(props.duration().as_millis()).ok();
    let codec = Some(format!("{:?}", tagged.file_type()).to_ascii_lowercase());

    let mut m = AudioMeta {
        bitrate_kbps: props.audio_bitrate().map(i64::from),
        sample_rate_hz: props.sample_rate().map(i64::from),
        channels: props.channels().map(i64::from),
        bit_depth: props.bit_depth().map(i64::from),
        ..Default::default()
    };

    // The primary tag is the format's native one (ID3v2 for MP3, Vorbis
    // comments for FLAC); falling back to the first of any type is what
    // picks up a file carrying only an ID3v1 or APEv2 block.
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        m.title = tag.title().map(|s| s.into_owned());
        m.artist = tag.artist().map(|s| s.into_owned());
        m.album = tag.album().map(|s| s.into_owned());
        m.genre = tag.genre().map(|s| s.into_owned());
        m.album_artist = tag.get_string(ItemKey::AlbumArtist).map(str::to_string);
        m.composer = tag.get_string(ItemKey::Composer).map(str::to_string);
        m.track_no = tag.track().map(i64::from);
        m.track_total = tag.track_total().map(i64::from);
        m.disc_no = tag.disk().map(i64::from);
        m.disc_total = tag.disk_total().map(i64::from);
        // Prefer the raw tag text over lofty's parsed `Timestamp`,
        // which would collapse `1997` and `1997-01-01` to one value.
        m.date = tag
            .get_string(ItemKey::RecordingDate)
            .or_else(|| tag.get_string(ItemKey::Year))
            .or_else(|| tag.get_string(ItemKey::ReleaseDate))
            .map(str::to_string)
            .or_else(|| tag.date().map(|d| d.to_string()));
    }
    Ok((m, duration_ms, codec))
}

// ─────────────────────────────────────────────────────────────────────
// EXIF

fn read_exif(path: &Path) -> Result<VisualMeta> {
    use exif::{In, Tag};

    let file = std::fs::File::open(path)?;
    let mut r = std::io::BufReader::new(&file);
    let exif = exif::Reader::new().read_from_container(&mut r)?;

    let text = |tag: Tag| -> Option<String> {
        let f = exif.get_field(tag, In::PRIMARY)?;
        let s = match &f.value {
            // ASCII values arrive as raw byte strings; the display form
            // would wrap them in quotes.
            exif::Value::Ascii(v) => v
                .first()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())?,
            _ => f.display_value().to_string(),
        };
        let s = s.trim().trim_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let uint = |tag: Tag| -> Option<i64> {
        exif.get_field(tag, In::PRIMARY)?
            .value
            .get_uint(0)
            .map(i64::from)
    };
    let rational = |tag: Tag| -> Option<f64> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            exif::Value::Rational(v) => v.first().map(|r| r.to_f64()),
            exif::Value::SRational(v) => v.first().map(|r| r.to_f64()),
            _ => None,
        }
    };

    let mut m = VisualMeta {
        width: uint(Tag::PixelXDimension).or_else(|| uint(Tag::ImageWidth)),
        height: uint(Tag::PixelYDimension).or_else(|| uint(Tag::ImageLength)),
        orientation: uint(Tag::Orientation).filter(|v| (1..=8).contains(v)),
        camera_make: text(Tag::Make),
        camera_model: text(Tag::Model),
        lens_model: text(Tag::LensModel),
        iso: uint(Tag::PhotographicSensitivity).or_else(|| uint(Tag::ISOSpeed)),
        exposure_time: text(Tag::ExposureTime),
        f_number: rational(Tag::FNumber),
        focal_length_mm: rational(Tag::FocalLength),
        title: text(Tag::ImageDescription),
        caption: text(Tag::UserComment),
        ..Default::default()
    };

    // GPS is three rationals (degrees, minutes, seconds) plus a
    // hemisphere letter in a separate tag. Losing the ref is how you
    // end up with every southern-hemisphere photo in the north.
    m.gps_lat = gps_degrees(&exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S');
    m.gps_lon = gps_degrees(&exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W');
    m.gps_altitude_m = rational(Tag::GPSAltitude).map(|alt| {
        // GPSAltitudeRef is 1 for "below sea level".
        if exif
            .get_field(Tag::GPSAltitudeRef, In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            == Some(1)
        {
            -alt
        } else {
            alt
        }
    });

    m.captured_at = exif_timestamp(&exif);
    Ok(m)
}

fn gps_degrees(
    exif: &exif::Exif,
    value: exif::Tag,
    reference: exif::Tag,
    negative: u8,
) -> Option<f64> {
    use exif::In;
    let f = exif.get_field(value, In::PRIMARY)?;
    let exif::Value::Rational(parts) = &f.value else {
        return None;
    };
    if parts.len() < 3 {
        return None;
    }
    let deg = parts[0].to_f64() + parts[1].to_f64() / 60.0 + parts[2].to_f64() / 3600.0;
    let sign = match exif.get_field(reference, In::PRIMARY).map(|r| &r.value) {
        Some(exif::Value::Ascii(v)) if v.first().and_then(|b| b.first()) == Some(&negative) => -1.0,
        _ => 1.0,
    };
    Some(deg * sign)
}

/// `DateTimeOriginal` as ISO-8601, with `OffsetTimeOriginal` appended
/// when the camera recorded one.
fn exif_timestamp(exif: &exif::Exif) -> Option<String> {
    use exif::{In, Tag};
    let ascii = |tag: Tag| -> Option<String> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            exif::Value::Ascii(v) => v
                .first()
                .map(|b| String::from_utf8_lossy(b).trim().to_string()),
            _ => None,
        }
    };
    let (raw, offset) = match ascii(Tag::DateTimeOriginal) {
        Some(v) => (v, ascii(Tag::OffsetTimeOriginal)),
        None => (ascii(Tag::DateTime)?, ascii(Tag::OffsetTime)),
    };
    // EXIF writes `2026:05:04 03:42:05`; ISO-8601 wants
    // `2026-05-04T03:42:05`.
    let (date, time) = raw.split_once(' ')?;
    if date.len() != 10 || time.len() < 8 {
        return None;
    }
    let iso = format!("{}T{}", date.replace(':', "-"), &time[..8]);
    Some(match offset {
        Some(o) if !o.trim().is_empty() && o.trim() != "\0" => format!("{iso}{}", o.trim()),
        _ => iso,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Container structure: what EXIF does not carry

fn read_structure(
    path: &Path,
    container: Container,
    visual: &mut VisualMeta,
    meta: &mut Meta,
) -> Result<()> {
    let mut src = Src::open(path)?;
    match container {
        Container::Png => read_png_ihdr(&mut src, visual)?,
        Container::Jpeg => read_jpeg_sof(&mut src, visual)?,
        Container::Bmff => read_bmff(&mut src, visual, meta)?,
        Container::Avi => read_avi(&mut src, visual, meta)?,
        // A DNG's primary IFD is usually the embedded preview, so the
        // EXIF pass above reported the preview's size. Override it with
        // the full-resolution IFD's — the same one the payload hash
        // covers. See `payload::tiff::dimensions`.
        Container::Tiff => {
            if let Some((w, h)) = tiff::dimensions(&mut src)? {
                visual.width = Some(w);
                visual.height = Some(h);
            }
        }
        _ => {}
    }
    Ok(())
}

/// PNG carries its dimensions in `IHDR`, which is always the first
/// chunk. Cheaper and more reliable than the optional `eXIf` block.
fn read_png_ihdr(src: &mut Src, visual: &mut VisualMeta) -> Result<()> {
    let b = src.read_upto(8, 16)?;
    if b.len() < 16 || &b[4..8] != b"IHDR" {
        return Ok(());
    }
    visual.width = super::payload::be_u32(&b, 8).map(i64::from);
    visual.height = super::payload::be_u32(&b, 12).map(i64::from);
    Ok(())
}

/// A JPEG's true dimensions live in its `SOF` marker. EXIF's
/// `PixelXDimension` usually agrees, but is absent on JPEGs with no
/// EXIF block and *stale* on some that were cropped by a tool that
/// forgot to update it — so the frame header wins where they differ.
fn read_jpeg_sof(src: &mut Src, visual: &mut VisualMeta) -> Result<()> {
    let mut at = 2u64;
    while at + 4 <= src.len() {
        let h = src.read_at(at, 2)?;
        if h[0] != 0xff {
            break;
        }
        let marker = h[1];
        if marker == 0xff {
            at += 1;
            continue;
        }
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            at += 2;
            continue;
        }
        let Some(len) = super::payload::be_u16(&src.read_upto(at + 2, 2)?, 0).map(u64::from) else {
            break;
        };
        if len < 2 {
            break;
        }
        // Every SOF marker except DHT (0xc4), JPG (0xc8) and DAC (0xcc).
        let is_sof =
            (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc;
        if is_sof && len >= 7 {
            let b = src.read_at(at + 4, 5)?;
            visual.height = super::payload::be_u16(&b, 1).map(i64::from);
            visual.width = super::payload::be_u16(&b, 3).map(i64::from);
            return Ok(());
        }
        at += 2 + len;
    }
    Ok(())
}

/// Seconds between 1904-01-01 (QuickTime's epoch) and 1970-01-01.
const QT_EPOCH_OFFSET: i64 = 2_082_844_800;

fn read_bmff(src: &mut Src, visual: &mut VisualMeta, meta: &mut Meta) -> Result<()> {
    let top = bmff::atoms(src, 0, src.len())?;
    let Some(moov) = bmff::find(&top, b"moov").copied() else {
        return Ok(());
    };
    let kids = bmff::atoms(src, moov.body_at, moov.body_at + moov.body_len)?;

    if let Some(mvhd) = bmff::find(&kids, b"mvhd") {
        let b = src.read_at(mvhd.body_at, mvhd.body_len.min(32))?;
        let v64 = b.first().copied() == Some(1);
        let (created, timescale, duration) = if v64 {
            (
                super::payload::be_u64(&b, 4).map(|v| v as i64),
                super::payload::be_u32(&b, 20).map(i64::from),
                super::payload::be_u64(&b, 24).map(|v| v as i64),
            )
        } else {
            (
                super::payload::be_u32(&b, 4).map(i64::from),
                super::payload::be_u32(&b, 12).map(i64::from),
                super::payload::be_u32(&b, 16).map(i64::from),
            )
        };
        if let (Some(ts), Some(d)) = (timescale, duration) {
            if ts > 0 {
                meta.duration_ms = Some(d.saturating_mul(1000) / ts);
            }
        }
        // Spec says UTC. Many cameras write local time here anyway,
        // which is why the `©day` tag below takes precedence when the
        // file has one — it carries a real offset.
        if let Some(c) = created.filter(|c| *c > QT_EPOCH_OFFSET) {
            visual.captured_at = visual.captured_at.take().or_else(|| {
                chrono::DateTime::from_timestamp(c - QT_EPOCH_OFFSET, 0)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
            });
        }
    }

    for trak in kids.iter().filter(|a| &a.btype == b"trak") {
        read_bmff_track(src, *trak, visual)?;
    }

    if let Some(udta) = bmff::find(&kids, b"udta").copied() {
        read_bmff_udta(src, udta, visual)?;
    }
    Ok(())
}

fn read_bmff_track(src: &mut Src, trak: bmff::Atom, visual: &mut VisualMeta) -> Result<()> {
    let kids = bmff::atoms(src, trak.body_at, trak.body_at + trak.body_len)?;

    // `tkhd`'s width/height are 16.16 fixed point, and are zero on
    // audio tracks — which is what distinguishes the two here.
    if let Some(tkhd) = bmff::find(&kids, b"tkhd") {
        let b = src.read_at(tkhd.body_at, tkhd.body_len.min(96))?;
        // Body layout: version/flags(4) creation modification track_id
        // reserved(4) duration reserved(8) layer+alt(4) volume+res(4)
        // matrix(36), then the two 16.16 fixed-point dimensions. The
        // 64-bit variant widens creation, modification and duration by
        // 4 bytes each, which is the whole 12-byte difference.
        let base = if b.first().copied() == Some(1) {
            88
        } else {
            76
        };
        if let (Some(w), Some(h)) = (
            super::payload::be_u32(&b, base),
            super::payload::be_u32(&b, base + 4),
        ) {
            let (w, h) = ((w >> 16) as i64, (h >> 16) as i64);
            if w > 0 && h > 0 {
                visual.width = Some(w);
                visual.height = Some(h);
            }
        }
    }

    let Some(stbl) = bmff::descend(src, trak, &[b"mdia", b"minf", b"stbl"])? else {
        return Ok(());
    };
    let sb = bmff::atoms(src, stbl.body_at, stbl.body_at + stbl.body_len)?;
    let Some(stsd) = bmff::find(&sb, b"stsd") else {
        return Ok(());
    };
    // FullBox(4) + entry_count(4) + the first entry's size(4), then its
    // four-CC format.
    let b = src.read_at(stsd.body_at, stsd.body_len.min(20))?;
    let Some(fourcc) = b.get(12..16) else {
        return Ok(());
    };
    let name = String::from_utf8_lossy(fourcc).trim().to_ascii_lowercase();
    // Video sample entries are the ones on a track with real
    // dimensions; everything else on a media file is the sound.
    if VIDEO_FOURCC.iter().any(|v| name.starts_with(v)) {
        visual.video_codec = Some(name);
    } else {
        visual.audio_codec = Some(name);
    }
    Ok(())
}

/// Sample-entry four-CCs that mean "this track is picture". Prefix
/// matches, because the versioned variants (`avc1`/`avc3`,
/// `hvc1`/`hev1`) all share a stem.
const VIDEO_FOURCC: &[&str] = &["avc", "hvc", "hev", "mp4v", "av01", "vp09", "jpeg", "dvh"];

fn read_bmff_udta(src: &mut Src, udta: bmff::Atom, visual: &mut VisualMeta) -> Result<()> {
    let kids = bmff::atoms(src, udta.body_at, udta.body_at + udta.body_len)?;
    for a in &kids {
        // `©xyz`: ISO-6709 location, which is where an iPhone puts a
        // video's coordinates. Format is `+37.7749-122.4194+010.000/`,
        // behind a 4-byte length + language pair.
        if a.btype == [0xa9, b'x', b'y', b'z'] && a.body_len >= 4 {
            let b = src.read_at(a.body_at + 4, a.body_len - 4)?;
            if let Some((lat, lon, alt)) = parse_iso6709(&String::from_utf8_lossy(&b)) {
                visual.gps_lat = visual.gps_lat.or(Some(lat));
                visual.gps_lon = visual.gps_lon.or(Some(lon));
                visual.gps_altitude_m = visual.gps_altitude_m.or(alt);
            }
        }
        // `©day`: an ISO-8601 string, usually with a real offset. It
        // beats `mvhd` for exactly that reason.
        if a.btype == [0xa9, b'd', b'a', b'y'] && a.body_len >= 4 {
            let b = src.read_at(a.body_at + 4, a.body_len - 4)?;
            let s = String::from_utf8_lossy(&b)
                .trim()
                .trim_matches('\0')
                .to_string();
            if s.len() >= 10 {
                visual.captured_at = Some(s);
            }
        }
    }
    Ok(())
}

/// ISO-6709: sign-prefixed fields run together, e.g.
/// `+37.7749-122.4194+010.000/`.
fn parse_iso6709(s: &str) -> Option<(f64, f64, Option<f64>)> {
    let s = s.trim().trim_end_matches('/');
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if (c == '+' || c == '-') && !cur.is_empty() {
            fields.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        fields.push(cur);
    }
    if fields.len() < 2 {
        return None;
    }
    let lat = fields[0].parse::<f64>().ok()?;
    let lon = fields[1].parse::<f64>().ok()?;
    let alt = fields.get(2).and_then(|a| a.parse::<f64>().ok());
    Some((lat, lon, alt))
}

fn read_avi(src: &mut Src, visual: &mut VisualMeta, meta: &mut Meta) -> Result<()> {
    use super::payload::riff;
    for c in riff::chunks(src)? {
        if &c.id != b"LIST" || c.data_len < 4 {
            continue;
        }
        if src.read_at(c.data_at, 4)? != b"hdrl" {
            continue;
        }
        // `avih` is the first chunk inside the `hdrl` list.
        let b = src.read_at(c.data_at + 4, c.data_len.saturating_sub(4).min(64))?;
        if !b.starts_with(b"avih") {
            continue;
        }
        let f = |at: usize| super::payload::le_u32(&b, 8 + at).map(i64::from);
        // microseconds per frame, …, total frames, …, width, height
        let us_per_frame = f(0);
        let total_frames = f(16);
        visual.width = f(32).filter(|v| *v > 0);
        visual.height = f(36).filter(|v| *v > 0);
        if let Some(us) = us_per_frame.filter(|v| *v > 0) {
            visual.frame_rate = Some(1_000_000.0 / us as f64);
            if let Some(n) = total_frames {
                meta.duration_ms = Some(n.saturating_mul(us) / 1000);
            }
        }
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso6709_splits_on_the_sign_not_on_whitespace() {
        assert_eq!(
            parse_iso6709("+37.7749-122.4194+010.000/"),
            Some((37.7749, -122.4194, Some(10.0)))
        );
        assert_eq!(
            parse_iso6709("-33.8688+151.2093/"),
            Some((-33.8688, 151.2093, None))
        );
        assert_eq!(parse_iso6709(""), None);
        assert_eq!(parse_iso6709("+37.7749"), None);
        assert_eq!(parse_iso6709("garbage"), None);
    }

    #[test]
    fn video_fourcc_prefixes_cover_the_versioned_variants() {
        for name in ["avc1", "avc3", "hvc1", "hev1", "av01", "vp09", "mp4v"] {
            assert!(
                VIDEO_FOURCC.iter().any(|v| name.starts_with(v)),
                "{name} should be recognized as video"
            );
        }
        for name in ["mp4a", "alac", "sowt", "twos", "ac-3"] {
            assert!(
                !VIDEO_FOURCC.iter().any(|v| name.starts_with(v)),
                "{name} should not be recognized as video"
            );
        }
    }
}
