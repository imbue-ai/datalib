# Upgrade on launch: each step answers for its own store

**Status: built on `claude/upgrade-multi-source-sync-a34fe8`
(imbue-ai/datalib#1074), 2026-10-08.** The reference is the dag README
§ "Upgrading a root" and `step_protocol.md` § Migrate; this file records
why it is shaped that way.

## 1. The problem

A new build can change the shape ("layout") of what a step wrote: a raw
store's tables (reached through the provider's migration ladder when the
store is next opened to write), a render store's, the grid index's
(reached only by running the step again).

Before this, the runner reconciled shapes lazily, inside whatever sync
the person pressed. It held a copy of the render and index DDL hashes
(`BUILTIN_STORE_SHAPES`), put them in those steps' fingerprints, and
pulled any writer of a store in an old shape into the scope of a request
that reached a reader of it. So pressing Sync on one source after an
upgrade re-rendered every source, and those renders read raw stores that
only their own download would have migrated (#1053's column rename
failed them; #1055 patched that one rename inside render).

## 2. The design

**The runner knows nothing about any store.** It keeps one fact about
itself: which builds have finished a launch pass on this root
(`launch_passes` in `system/supervisor.sqlite`). The first time a build
runs, before the loop takes any request, it asks every step that takes
the verb, producers first, by appending `--migrate` to its command line.
The step decides, and answers in its outcome:

- **nothing to do** — its store is current, or it has none yet;
- **migrated** — a raw store climbed its ladder in place, fetching
  nothing; the step reports the new version, which the record takes
  while keeping the step's last success;
- **needs a rerun** — a render store or the index is in a shape it
  cannot reach in place; the record keeps `steps.needs_rerun`, the tick
  holds the step due until it next succeeds, and the app offers to run
  it ("Your rendered documents are out of date. Re-render now?").

Which steps take the verb is a capability the loader sets
(`StepSpec::migrates`, for `datalib-step`'s own steps), not knowledge of
storage. `BUILTIN_STORE_SHAPES`, `StepSpec::store_shape`,
`steps.store_shape` and the old-shape rules in the tick are gone. A
request's scope is its roots and what reads them, nothing more.

**Verbs are flags, not environment variables** (`--reset <part>`,
`--migrate`). A command that has never heard of a verb then refuses it
instead of running a sync, and nothing the step starts inherits it.

**The UI.** While the pass runs `/api/config` says so
(`upgrade.migrating`, a row per step), and the app shows a blocking
screen with one row per source. After it, `upgrade.rerender`
(`round::rerun_offer`) is what the dialog offers; yes opens one request
per group rooted there, `opened_by = "upgrade"`.

**Gone with it:** the render fallback for the pre-#1053 problems column.
A render reads the current raw shape or fails loudly.

## 3. Tests

1. **Every source type can be migrated from every past release**
   (`datalib_step/raw_shapes/`, `migrate.rs`): this tree's raw shape per
   source type, and 0.37.0, 0.39.0 and 0.40.0's, dumped from those tags.
   Each is built with a row in every table (an open rebuilds an empty
   table whatever its shape) and migrated; with the shared ladder's
   rename rung removed it fails for all 27 types. A release copies
   `current.json` forward.
2. **Each function's answer** (`migrate.rs`): a render store and the
   index in this build's shape answer nothing, in another `needs_rerun`.
3. **The protocol** (`subprocess.rs`): `--reset` and `--migrate` arrive
   on argv; a migrated version is recorded with the last success kept;
   `needs_rerun` is recorded and offered.
4. **The tick and the round**: a step that needs a rerun runs only for a
   request that reaches it; the offer names it and its success clears it.
5. **The app** (`http_tests/upgrade_on_launch.rs`): every step is asked,
   producers first, before a sync opened at boot runs; the offer lists
   what answered; the next launch of the same build asks nobody.
6. **The UI** (Playwright `upgrade-on-launch.spec.ts`): the screen, the
   dialog, yes and not now.

## 4. Not done

- A real upgrade end to end: a release publishes the TNG fixture's raw
  stores; the next build migrates and renders them and checks that a
  playback download afterwards owes nothing.
- A custom command cannot yet declare that it takes `--migrate`.
