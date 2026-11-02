//! `NormalizedPhoto`: a photo or video as one source describes it — a
//! Lightroom catalog's image, later an Apple Photos asset. Only the shape:
//! a render crate fills it from raw rows, the render store keeps it with
//! the photo's document, and the `photos` applet reads nothing else
//! (`docs/dev/plans/photos_applet.md`).

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedPhoto {
    /// The photo's grid row, which is its own document.
    pub uuid: String,
    pub source_id: String,
    pub kind: MediaKind,
    pub title: Option<String>,
    pub headline: Option<String>,
    pub caption: Option<String>,
    pub copyright: Option<String>,
    pub creator: Option<String>,
    /// When it was taken, with the offset `offset_from` says was used.
    pub captured_at: Option<String>,
    pub offset_from: Option<OffsetFrom>,
    /// The last edit in the source's own app, an instant.
    pub edited_at_utc: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub exposure: Exposure,
    /// 0–5 stars; `None` when unrated.
    pub rating: Option<u8>,
    /// 1 picked, -1 rejected, 0 neither.
    pub pick: Option<i8>,
    pub color_label: Option<String>,
    /// Each keyword as its path from the top of the keyword tree, the
    /// keyword itself last. People are not here: they are in `people`.
    pub keywords: Vec<Vec<String>>,
    pub people: Vec<Person>,
    pub albums: Vec<String>,
    pub place: Place,
    pub geo: Option<Geo>,
    /// The H3 cells `geo` falls in, coarsest first.
    pub h3: Vec<H3Cell>,
    /// The picture's size in pixels as stored, before any crop.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub file_name: String,
    /// The folder it is filed in, as the source names it.
    pub folder: Option<String>,
    pub original: Original,
    pub pictures: Pictures,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Video,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// Where the offset of `captured_at` came from: the photo's own metadata,
/// or the zone the source's config names, which is a guess about where
/// the photo was taken.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OffsetFrom {
    /// The offset on the metadata's original capture time.
    DateTimeOriginal,
    /// The offset on the metadata's creation time, where the capture
    /// time carries none.
    CreateDate,
    /// The source's configured `capture_time_zone`.
    ConfiguredZone,
    /// Nothing said: the time is read as UTC, and the source reports it.
    AssumedUtc,
}

impl OffsetFrom {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// Whether the offset is the photo's own rather than assumed.
    pub fn is_recorded(self) -> bool {
        match self {
            OffsetFrom::DateTimeOriginal | OffsetFrom::CreateDate => true,
            OffsetFrom::ConfiguredZone | OffsetFrom::AssumedUtc => false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    pub iso: Option<f64>,
    pub shutter_seconds: Option<f64>,
    pub aperture: Option<f64>,
    pub focal_length_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    /// Where their face is, when it was tagged on a face rather than
    /// only as a keyword.
    pub face: Option<Rect>,
}

/// A rectangle in 0–1 picture coordinates, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    /// A landmark or spot: "Golden Gate Bridge".
    pub location: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
}

impl Place {
    /// The names, the most specific first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        [&self.location, &self.city, &self.state, &self.country]
            .into_iter()
            .filter_map(|n| n.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geo {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct H3Cell {
    pub resolution: u8,
    /// The cell's id as H3 writes it: 15 lowercase hex digits.
    pub cell: String,
}

/// The original file. `relative_to_catalog` is for a library that moved
/// with its catalog, and is tried when `absolute` is not there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Original {
    pub absolute: Option<String>,
    pub relative_to_catalog: Option<String>,
}

/// How to find the source's own rendered pictures of the photo at the
/// moment someone asks: their file names change on every edit, so none is
/// stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pictures {
    /// A Lightroom Classic catalog's `Previews.lrdata`, looked up in its
    /// `previews.db` by the image's `id_local`.
    Lightroom {
        /// The `.lrcat`; its previews are the `<name> Previews.lrdata`
        /// beside it. `None` when nothing says where it is.
        catalog: Option<String>,
        image_id_local: i64,
        file_uuid: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strum_and_serde_agree() {
        for k in MediaKind::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(MediaKind::parse(k.as_str()), Some(*k));
        }
        for o in OffsetFrom::VARIANTS {
            assert_eq!(serde_json::to_value(o).unwrap(), o.as_str());
            assert_eq!(OffsetFrom::parse(o.as_str()), Some(*o));
        }
    }
}
