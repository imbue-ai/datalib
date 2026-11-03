//! The H3 behaviour datalib depends on, one test per fact, in the order
//! `datalib/backend/h3/README.md` states them. An `h3o` bump that moves a
//! fact fails here by name: fix the fact in the README, then the test,
//! then whatever leaned on it.
//!
//! Points are drawn from a fixed-seed generator, so a run is repeatable
//! and covers the whole globe, both poles' neighbourhoods and the
//! antimeridian included.

use datalib_h3::{
    ancestor, finest_range, locate, neighbourhood_ranges, stored, CellIndex, Resolution, FINEST,
};

const POINTS: usize = 20_000;

/// xorshift64*: enough spread for sampling, and no dependency.
struct Points(u64);

impl Points {
    fn next_unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Iterator for Points {
    type Item = (f64, f64);
    fn next(&mut self) -> Option<(f64, f64)> {
        // Uniform on the sphere, not on the lat/lng rectangle.
        let latitude = (2.0 * self.next_unit() - 1.0).asin().to_degrees();
        let longitude = 360.0 * self.next_unit() - 180.0;
        Some((latitude, longitude))
    }
}

fn points() -> impl Iterator<Item = (f64, f64)> {
    Points(0x9e37_79b9_7f4a_7c15).take(POINTS)
}

fn coarser() -> impl Iterator<Item = Resolution> {
    Resolution::range(Resolution::Zero, Resolution::Fourteen)
}

/// The twelve pentagons at a resolution, where H3's numbering skips a
/// branch of children: the case a range could get wrong.
fn pentagons(resolution: Resolution) -> Vec<CellIndex> {
    resolution.pentagons().collect()
}

/// Fact 1: every cell's integer is positive as an `i64`, so a SQLite
/// INTEGER column holds it in order. Among cells of one resolution that
/// order is `h3o`'s too; across resolutions it is not (`h3o` compares
/// with the resolution bits cleared), so a column holds one resolution.
#[test]
fn one_resolution_sorts_alike_as_integers_and_as_cells() {
    let mut finest: Vec<CellIndex> = points().filter_map(|(a, b)| locate(a, b)).collect();
    finest.extend(pentagons(FINEST));
    let mut every = finest.clone();
    every.extend(CellIndex::base_cells());
    assert!(every.iter().all(|c| stored(*c) > 0));

    let mut by_cell = finest.clone();
    by_cell.sort();
    let mut by_stored = finest;
    by_stored.sort_by_key(|c| stored(*c));
    assert_eq!(by_cell, by_stored);

    // A cell and its centre child at the finest resolution: `h3o` reads
    // the parent's unused digits (all 7) and puts the child first; the
    // integers read the parent's lower resolution and put it first.
    let base = CellIndex::base_cells().nth(20).unwrap();
    let child = base.center_child(FINEST).unwrap();
    assert!(child < base);
    assert!(stored(base) < stored(child));
}

/// Fact 2: the finest cells inside a cell are one run of integers. Every
/// point's finest cell lies in the range of each of its ancestors.
#[test]
fn a_finest_cell_is_inside_every_ancestors_range() {
    for (latitude, longitude) in points() {
        let finest = locate(latitude, longitude).unwrap();
        for r in coarser() {
            let parent = ancestor(finest, r).unwrap();
            assert!(
                finest_range(parent).contains(&stored(finest)),
                "{finest} is not in the range of its parent {parent}"
            );
        }
    }
}

/// Fact 2, the other half: nothing just outside the run is inside the
/// cell, so the range is exact, pentagons included.
#[test]
fn the_range_is_exact_at_both_ends() {
    let mut cells: Vec<CellIndex> = points()
        .take(500)
        .filter_map(|(a, b)| locate(a, b))
        .flat_map(|c| coarser().map(move |r| ancestor(c, r).unwrap()))
        .collect();
    for r in coarser() {
        cells.extend(pentagons(r));
    }
    for cell in cells {
        let range = finest_range(cell);
        let first = datalib_h3::from_stored(*range.start()).unwrap();
        let last = datalib_h3::from_stored(*range.end()).unwrap();
        assert_eq!(first.parent(cell.resolution()), Some(cell));
        assert_eq!(last.parent(cell.resolution()), Some(cell));
        for outside in [first.pred(), last.succ()].into_iter().flatten() {
            assert_ne!(
                outside.parent(cell.resolution()),
                Some(cell),
                "{outside} in {cell}"
            );
            assert!(!range.contains(&stored(outside)));
        }
    }
}

/// Fact 3: inside a cell means inside it by H3's parent links, not on the
/// map. Locating a point directly at a coarse resolution names a different
/// cell than its finest cell's parent for a few percent of points, at
/// every resolution. If this ever reads zero, the README's warning can go.
#[test]
fn a_coarse_cell_by_parent_is_not_always_the_one_by_location() {
    for r in coarser() {
        let differ = points()
            .filter(|&(latitude, longitude)| {
                let point = h3o::LatLng::new(latitude, longitude).unwrap();
                let finest = point.to_cell(FINEST);
                point.to_cell(r) != ancestor(finest, r).unwrap()
            })
            .count();
        let share = differ as f64 / POINTS as f64;
        assert!(
            (0.02..0.15).contains(&share),
            "resolution {r}: {differ} of {POINTS} points differ"
        );
    }
}

/// Fact 4: where they differ, the parent is a neighbour of the cell the
/// point is in on the map. So a search of a cell and its six neighbours
/// finds every point that lies in the cell.
#[test]
fn the_parent_is_the_located_cell_or_a_neighbour() {
    for (latitude, longitude) in points() {
        let point = h3o::LatLng::new(latitude, longitude).unwrap();
        let finest = point.to_cell(FINEST);
        for r in coarser() {
            let on_the_map = point.to_cell(r);
            let disk: Vec<CellIndex> = on_the_map.grid_disk(1);
            let parent = ancestor(finest, r).unwrap();
            assert!(
                disk.contains(&parent),
                "({latitude}, {longitude}) at {r}: parent {parent} is not next to {on_the_map}"
            );
            assert!(neighbourhood_ranges(on_the_map)
                .iter()
                .any(|range| range.contains(&stored(finest))));
        }
    }
}

/// Fact 5: a cell's name is 15 lowercase hex digits at every resolution,
/// so names sort as the integers do.
#[test]
fn every_name_is_fifteen_hex_digits() {
    for (latitude, longitude) in points().take(1000) {
        let finest = locate(latitude, longitude).unwrap();
        for r in Resolution::range(Resolution::Zero, FINEST) {
            let name = ancestor(finest, r).unwrap().to_string();
            assert_eq!(name.len(), 15, "{name}");
            assert_eq!(name, name.to_lowercase());
            assert_eq!(datalib_h3::parse(&name), ancestor(finest, r));
        }
    }
}

/// Fact 6: SQLite answers `BETWEEN` on an indexed INTEGER column with a
/// range search of the index, not a scan of the table.
#[tokio::test]
async fn sqlite_serves_a_range_from_the_index() {
    use sqlx::{Connection, Row};
    let mut db = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    for sql in [
        "CREATE TABLE rows (uuid TEXT PRIMARY KEY, h3 INTEGER)",
        "CREATE INDEX rows_by_h3 ON rows (h3)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    let finest = locate(37.8199, -122.4783).unwrap();
    let range = finest_range(ancestor(finest, Resolution::Six).unwrap());
    let plan: Vec<String> =
        sqlx::query("EXPLAIN QUERY PLAN SELECT uuid FROM rows WHERE h3 BETWEEN ? AND ?")
            .bind(range.start())
            .bind(range.end())
            .fetch_all(&mut db)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect();
    assert_eq!(
        plan,
        ["SEARCH rows USING COVERING INDEX rows_by_h3 (h3>? AND h3<?)"]
    );
}
