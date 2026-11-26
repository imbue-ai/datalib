# A curated overlay: a person's own stores over synced data

`datalib_curated/` holds what a person made and nothing can rebuild:
their contacts (`datalib_contacts/contacts.doltlite_db`) and the problem
rows they muted (`datalib_problem_mutes/mutes.doltlite_db`). Every sync
rebuilds the rest of a root from upstream; these stores come through it
as they were. After an upgrade, the new build opens files the previous
one wrote, and has to bring them forward and fit them to the new data.

A test that makes its contacts afresh each run never sees that: the
store is always in this build's shape, and its links were made to fit
the data in front of them. So two tests start from **checked-in
stores** and lay them over a root. Each is made in the current shape
and **carried forward**: a store stays as committed until a change
moves its shape, and that change then upgrades the committed file and
writes the result back (step 5). So the next change that moves a shape
opens the file in the shape before it, as a person's library would be,
and proves that one step of the upgrade:

| test | the stores | the root |
|---|---|---|
| `:tng_overlay_test` here | `tests/fixtures/curated/datalib_curated/`, Picard's | `materialize_tng_root`, the TNG corpus |
| `//datalib/backend/dag:manual_e2e_live_sync_golden` | `datalib_curated/` in the private test-data repo, beside `dag.toml` | the live bake's three syncs |

Both go through this crate, in this order:

1. **`copy_in`** puts the checked-in stores into the root, and refuses a
   curated store it does not know, so a new one cannot go unchecked.
2. **`open_as_owners`** opens each the way its one writer does at
   launch: the contacts applet's `Store::open`, the index applet's open
   of the mutes. That climbs each migration ladder, as an upgrade would.
3. **`assert_every_table_has_a_row`**: a table with no row cannot show
   a missing rung, because an open rebuilds an empty table whatever
   shape it is in. `MAY_BE_EMPTY` names the one exception and why.
4. **`overlay`** asks the shipped search (`datalib-applet search`), as
   the app's search box does, what each store reaches, and fails if:
   - a contact, or a handle linked to one, finds nothing. A handle that
     only a source's record of a person holds (a Signal ACI) still
     fits: the person card shows that record.
   - a contact merged into another finds other rows than its survivor;
   - a group misses a member's rows;
   - `from:me` misses a row that one of the "me" contact's handles
     finds;
   - a mute names no problem the root has, or the Manage counts still
     count it.

   It answers with the sources each contact, handle and `from:` reaches,
   and each mute's problem, which both tests snapshot (`overlay.snap`)
   beside a dump of each store.
5. **`changed_by_this_build`** names each store whose rows or shape this
   build changed, leaving out the line naming the build that opened it.

What happens to a changed store is the one place the two differ. The
bake writes it back over the checked-in file, as it writes the config's
upgrade back over `dag.toml`, so the next bake starts from it. The TNG
test fails, so the change is read in review, and its `.update` target
writes the store back with the snapshots:

```sh
bazelisk run //datalib/backend/curated_overlay:tng_overlay_test.update
```

## Adding to a checked-in store

The stores are the fixture. Change them as a person would, through the
app's own routes, never by writing SQL into them:

1. Build a root: `bazelisk run //tests/fixtures:materialize_tng_root --
   <dir>` for TNG; for the bake, `cp -cR` the root the last bake left
   (it prints `[test] data_root = …`).
2. Copy the checked-in `datalib_curated/` into it.
3. Serve the two applets that write it, without `datalib-http`'s sync
   loop: `tests/fixtures/curated/serve_applets.sh <root> <state_dir>`.
4. Call their routes. `tests/fixtures/curated/curated_client.py` holds
   the calls; `curate_tng.py` beside it, and `curate_bake.py` in the
   test-data repo, are the records of what made each store, and the
   way to make one again from nothing. `GET /` on the contacts applet
   lists every route.
5. Stop the applets and copy `datalib_curated/*/*.doltlite_db` back.
   Run the test (the TNG `.update` target, or a bake) and read the
   snapshot diff.

Nothing from a real mirror goes into the TNG stores: they link the
corpus's made-up handles, and their photo is a solid square.
