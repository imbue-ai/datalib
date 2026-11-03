//! Where a thing is, as H3 cells, over `h3o`, which does all the geometry.
//! A location is kept as its finest cell, and a cell at any resolution is
//! one run of finest cells, so "inside this cell" is a `BETWEEN` on an
//! indexed integer column. README.md has the facts this rests on; each is
//! a test in `h3_facts`.

use std::ops::RangeInclusive;

pub use h3o::{CellIndex, Resolution};

/// The resolution every location is kept at: hexagons of about a square
/// metre.
pub const FINEST: Resolution = Resolution::Fifteen;

/// The finest cell holding a point, or `None` for a point off the globe.
/// `h3o` wraps any finite pair around the sphere, so a latitude of 91 is a
/// bad record, not a place.
pub fn locate(latitude: f64, longitude: f64) -> Option<CellIndex> {
    let on_the_globe = (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude);
    if !on_the_globe {
        return None;
    }
    Some(h3o::LatLng::new(latitude, longitude).ok()?.to_cell(FINEST))
}

/// A cell as a column stores it. H3 never sets the top bit, so the cast
/// keeps the order.
pub fn stored(cell: CellIndex) -> i64 {
    u64::from(cell) as i64
}

pub fn from_stored(value: i64) -> Option<CellIndex> {
    CellIndex::try_from(u64::try_from(value).ok()?).ok()
}

/// The stored finest cells inside `cell`: every one of them, and nothing
/// else that is a finest cell.
pub fn finest_range(cell: CellIndex) -> RangeInclusive<i64> {
    let last = cell.children_count(FINEST) - 1;
    let first = cell.child_at(0, FINEST).expect("a cell has a first child");
    let last = cell.child_at(last, FINEST).expect("and a last one");
    stored(first)..=stored(last)
}

/// The stored finest cells inside `cell` or any of its six neighbours, in
/// order: what holds every point that lies in `cell` on the map, which
/// [`finest_range`] alone misses near the edge (README §"Inside a cell").
pub fn neighbourhood_ranges(cell: CellIndex) -> Vec<RangeInclusive<i64>> {
    let mut disk: Vec<CellIndex> = cell.grid_disk(1);
    disk.sort_by_key(|c| stored(*c));
    disk.into_iter().map(finest_range).collect()
}

/// The cell holding `cell` at `resolution`, by H3's parent links. A coarse
/// cell is always found this way, never by locating the point again at
/// that resolution: the two disagree near a cell's edge, and only this one
/// agrees with [`finest_range`].
pub fn ancestor(cell: CellIndex, resolution: Resolution) -> Option<CellIndex> {
    cell.parent(resolution)
}

/// A cell written as H3 writes it, 15 lowercase hex digits; `None` for
/// anything else.
pub fn parse(text: &str) -> Option<CellIndex> {
    let hex = text.len() == 15 && text.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| text.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_off_the_globe_has_no_cell() {
        assert_eq!(locate(91.0, 0.0), None);
        assert_eq!(locate(0.0, 181.0), None);
        assert_eq!(locate(f64::NAN, 0.0), None);
        assert!(locate(37.8199, -122.4783).is_some());
    }

    #[test]
    fn a_stored_cell_reads_back() {
        let cell = locate(37.8199, -122.4783).unwrap();
        assert_eq!(from_stored(stored(cell)), Some(cell));
        assert_eq!(from_stored(-1), None);
        assert_eq!(from_stored(0), None);
    }

    /// A pentagon has five neighbours, every other cell six.
    #[test]
    fn a_neighbourhood_is_the_cell_and_its_ring() {
        let cell = ancestor(locate(37.8199, -122.4783).unwrap(), Resolution::Six).unwrap();
        let ranges = neighbourhood_ranges(cell);
        assert_eq!(ranges.len(), 7);
        assert!(ranges.contains(&finest_range(cell)));
        assert!(ranges.windows(2).all(|w| w[0].end() < w[1].start()));
        let pentagon = Resolution::Six.pentagons().next().unwrap();
        assert_eq!(neighbourhood_ranges(pentagon).len(), 6);
    }

    /// `place:` reads a value as a cell only when it is one, so a place
    /// name made of hex letters ("Cafe") stays a name.
    #[test]
    fn only_a_cell_parses_as_one() {
        let cell = locate(37.8199, -122.4783).unwrap();
        assert_eq!(parse(&cell.to_string()), Some(cell));
        assert_eq!(parse("cafe"), None);
        assert_eq!(parse("Golden Gate"), None);
        assert_eq!(parse("fffffffffffffff"), None);
    }
}
