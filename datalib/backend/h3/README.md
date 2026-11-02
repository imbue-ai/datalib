# `datalib_h3`: where a thing is

[H3](https://h3geo.org) cuts the globe into hexagons, at sixteen
resolutions from 0 (a hexagon the size of a continent) to 15 (about a
square metre). Each hexagon, a *cell*, is named by a 64-bit integer, and
each cell below resolution 0 has one *parent* at every coarser
resolution. [`h3o`](https://github.com/HydroniumLabs/h3o), a Rust port of
Uber's library (BSD-3-Clause), does all the geometry; this crate is the
few rules datalib keeps on top of it.

| resolution | a cell is about | as in |
|---|---|---|
| 4 | 1,770 km² | a region |
| 6 | 36 km² | a town |
| 8 | 0.74 km² | a neighbourhood |
| 10 | 15,000 m² | a block |
| 15 | 0.9 m² | where you stood |

## The rule: keep the finest cell, search by range

**A location is stored once, as its resolution-15 cell, in an indexed
INTEGER column** (`grid_rows.h3`, filled from a row's latitude and
longitude). A cell at any coarser resolution is then one unbroken run of
those integers, from its first resolution-15 descendant to its last
(`finest_range`), so "everything inside this cell" is

```sql
WHERE h3 BETWEEN :first AND :last
```

which SQLite answers by walking its index from `:first` to `:last` and
nothing else. One column serves every resolution; no row stores a cell
per resolution.

## Inside a cell: by parent, not on the map

Hexagons do not nest. Seven small ones never exactly cover a big one, so
H3 assigns each cell to one parent, and "inside" in the rule above means
*descended from*. Near a cell's edge that differs from the map: for a few
percent of points, the cell the point lies in at, say, resolution 6 is
not the resolution-6 parent of the cell it lies in at resolution 15.

Two consequences, and what we do about them:

- **Every coarse cell we show is found by parent** (`ancestor`), never by
  locating the point again at that resolution. Then a photo shown "in"
  a cell is always found by searching that cell.
- **A search that must not miss anything near the edge adds the
  neighbours.** The parent is always the cell the point lies in on the
  map or one of its six neighbours, so the seven ranges of
  `neighbourhood_ranges` hold every point that lies in the cell on the
  map, plus some just outside it. `near:<cell>` searches that way;
  `place:<cell>` searches by parent alone.

## The facts

Each is a test in `//datalib/backend/h3:h3_facts`, over 20,000 points
spread evenly over the globe. An `h3o` bump that moves one fails by name.

1. **Every cell's integer is positive as a signed 64-bit integer**, so a
   SQLite INTEGER column holds it in order. Among cells of one
   resolution that is also `h3o`'s order; across resolutions it is not
   (`h3o` compares as if the resolution were cleared), so a column holds
   one resolution.
2. **The resolution-15 cells inside a cell are one run of integers**,
   exactly: every point's finest cell is in the range of each of its
   ancestors, and the cells just before and after a range are outside
   the cell. That holds at the twelve pentagons too, where H3's
   numbering skips a branch.
3. **By parent and on the map disagree near the edge**: for between 2%
   and 15% of points at every resolution (about 6–7% in practice).
4. **The parent is the cell on the map or one of its neighbours.**
5. **A cell's name is 15 lowercase hex digits at every resolution**, so
   names sort as the integers do.
6. **SQLite serves `BETWEEN` on the indexed column from the index**: the
   plan is an index range search, not a table scan.
