# Raw-store shapes, by release

Each file here records the shape of every source type's raw store — its
tables, indexes and migration-ladder heights — as one build creates it.
`current.json` is this tree's. `<version>.json` is the shape a released
build left in people's data roots.

`migrate.rs`'s `every_recorded_raw_shape_migrates_to_this_build` builds a
store from each recorded shape, puts one made-up row in every table, and
migrates it the way a launch does (`docs/dev/plans/upgrade_on_launch.md`).
The row matters: an open rebuilds an empty table whatever shape it is
in, so only a table with something in it shows a missing rung. The test
fails, naming the release and the source type, when a change to a raw
table is neither additive nor carried by a rung on the provider's
ladder (`datalib/backend/etl/README.md` § "The migration ladder").

What it does not check: that a rung keeps what the rows *mean* (the
provider's own `tests/*/upgrade.rs` do that), the blob store beside the
entities store, and the tables a mirror or the Facebook and LinkedIn
walks create at run time.

- **After changing a raw table:** `bazel run
  //datalib/backend/datalib_step:raw_shapes.update` rewrites
  `current.json`; `the_current_raw_shapes_are_recorded` fails until you
  do.
- **At a release:** if `current.json` differs from the newest
  `<version>.json`, copy it to `<version of the release>.json`
  (`.claude/skills/release/SKILL.md` step 3). A release whose shapes
  match the one before needs no file.
- **A shape that should no longer be supported:** delete its file. A
  root last opened by that release then needs a reset of the sources
  that refuse.

`0.37.0.json` covers 0.36.1 and 0.37.0, `0.39.0.json` covers 0.38.0 and
0.39.0, which left the same shapes as each other; `0.40.0.json` is the
release after. They were recorded from those tags by the same dump, run
against each tag's own code. Releases before 0.36.1 wrote no
`_datalib_meta` and are not recorded.
