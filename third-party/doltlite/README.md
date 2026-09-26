# doltlite (bazel-vendored)

This directory wires **doltlite** — a SQLite fork with content-addressed
prolly-tree storage and `dolt_commit()` / `dolt_log()` SQL functions —
into the Rust build as a statically-linked dependency. After the build,
every binary that touches sqlx-sqlite ships doltlite inside itself; no
runtime `brew install`, no system libsqlite3 dependency.

## Dependency graph

```
   MODULE.bazel
       │
       │  http_archive(name="doltlite_amalgamation", sha256="…")
       ▼
   @doltlite_amalgamation//
       (extracted zip: doltlite.c + doltlite.h)
       │
       │  exports_files(...) from amalgamation.BUILD
       ▼
   //third-party/doltlite:rename_amalgamation
       (genrule: doltlite.{c,h}  →  sqlite3.{c,h})
       │
       ▼
   //third-party/doltlite:sqlite3
       (cc_library — compiles sqlite3.c into libsqlite3.a)
       │
       ├─────────────────────────────┐
       │                             │
       │  crate.annotation(          │  deps=[":sqlite3"]
       │    crate="libsqlite3-sys",  │
       │    deps=[":sqlite3"])       ▼
       │                     //third-party/doltlite:doltlite
       │                         (cc_binary — the CLI, for tests
       │                          and hand-inspecting raw stores)
       │                             ▲
       │                             │  srcs=[shell.c]
       │                     @doltlite_autoconf//
       │                         (tarball; we take only its
       │                          pre-generated ext/wasm/.../shell.c)
       │                             ▲
       │                             │  http_archive(name="doltlite_autoconf")
       ▼                        MODULE.bazel
   @datalib_crates//:libsqlite3-sys
   @datalib_crates//:sqlx-sqlite
   …all the way up to the binaries.
```

The two `http_archive`s must be pinned to the same doltlite version.
Nothing in Bazel couples them, and `:cli_version_test` does not
actually catch it either — see [Upgrading doltlite](#upgrading-doltlite).

## How caching works

Each arrow above is a Bazel action with its own cache entry:

- **Fetch** the zip: keyed on the http_archive `sha256`. Once per
  workstation, forever, until the pin changes.
- **Genrule** to rename: keyed on the source file digests. Trivial cost.
- **cc_library** compile: keyed on `sqlite3.c`'s digest + the C
  toolchain hermetic key. One ~30-second compile per (toolchain,
  doltlite-version) pair, then cached in `bazel-out/` and (if
  configured) on RBE.
- **libsqlite3-sys** Rust compile: pulls the cc_library output as a
  native dep. Recompiles only when libsqlite3-sys's source or our
  cc_library output moves.

In normal day-to-day edits to Rust code, none of these actions re-run.

## Upgrading doltlite

A version lives in **three** places and they must all move together:

| # | Location |
|---|----------|
| 1 | `MODULE.bazel` → `http_archive(name = "doltlite_amalgamation")` — the library |
| 2 | `MODULE.bazel` → `http_archive(name = "doltlite_autoconf")` — the CLI's `shell.c` |
| 3 | `BUILD.bazel` → `DOLTLITE_VERSION` |

and two files ride along: `LICENSE.md` and `APACHE_LICENSE`, upstream's
notices at the pinned version, which every release ships under
`licenses/doltlite/` (DoltLite is Apache-2.0). Neither archive carries
them, so re-fetch both from the release's tag when you bump.

`//tools:version_pins_test` checks that the three agree.
`:cli_version_test` does not, whatever its comments say; the check is
circular: the CLI prints the version it was
compiled with (`-DDOLTLITE_VERSION`, from #3), and the test compares
that against #3 again. Setting `DOLTLITE_VERSION = "0.11.52"` while
both archives are on 0.11.53 passes. What the test *does* genuinely
catch is worth keeping — that the CLI links and runs at all, and that
its dolt-SQL surface is real (it exercises `dolt_commit` and
`dolt_log`, so a shell accidentally linked against stock SQLite fails).
It just isn't a pin-drift guard.

Steps:

1. Find the new release: <https://github.com/dolthub/doltlite/releases>.
2. You need **two** assets, at the same version:
   - `doltlite-amalgamation-X.Y.Z.zip` — the library (`doltlite.{c,h}`).
   - `doltlite-autoconf-X.Y.Z.tar.gz` — for its pre-generated `shell.c`
     only. The amalgamation is library-only (no `main()`), so the CLI
     has to come from here.

   **Do not use any 0.11.x release before 0.11.4** — those amalgamation
   zips were broken and built stock SQLite, missing the prolly hooks.
3. Compute both sha256s:
   ```sh
   curl -fsSL <url> | shasum -a 256
   ```
4. Update `urls` + `sha256` + `strip_prefix` in **both** `MODULE.bazel`
   `http_archive`s — `doltlite_amalgamation` and `doltlite_autoconf`.
5. Bump the `DOLTLITE_VERSION` constant at the top of `BUILD.bazel`.
   It feeds `-DDOLTLITE_VERSION` into both the library and the CLI, so
   there's only one to change.
6. `bazelisk test //tools:version_pins_test
   //third-party/doltlite:cli_version_test` — the pins agree, and the
   CLI links and its dolt-SQL surface works against the new engine.
   Then `bazelisk test //...`: every store open goes through the new
   engine, and `//datalib/backend/etl:doltlite_two_process_test` is
   what says a reader still costs the writer nothing.

Before bumping, check whether the chunk-store format moved: grep
`CHUNK_STORE_VERSION` in the old and new `doltlite.c`. The open path
hard-rejects any mismatch (`SQLITE_NOTADB`, "written by an incompatible
doltlite version") with no migration path, so a bump there orphans every
existing `.doltlite_db` on disk rather than merely needing a rebuild.
It has been `12` from 0.11.13 through 0.50.12, and upstream freezes 12
for the DoltLite beta (`doc/doltlite/storage-format.md`): every version-12 file
stays readable and writable by later version-12 builds. So a bump that
stays on 12 needs no store migration.

Also worth a moment: a bump can move the *SQL surface's* semantics
without touching the storage format, and the tests that notice are the
ones asserting exact counts. 0.11.52 changed `dolt_diff.data_change` to
report `0` for a newly created **empty** table (it had been `1` through
0.11.51) — correct, but it moved a lightroom assertion from 113 tables
to the 38 that actually hold rows. If a bump fails a count assertion,
check whether upstream got *more* right before assuming a regression.
And by 0.50.12 doltlite registers the per-table modules (`dolt_at_<table>` and
friends) on first use rather than at open, so `pragma_module_list`
stopped listing them; `etl/src/pin.rs` had used that list to decide
what a commit holds, and every store open failed until it asked by
reading instead.

The C API does not move (it is a SQLite fork), so a bump needs no
wiring changes; the code changes it needs come from semantic shifts
like the two above.

## Files in this package

| Path                        | Purpose                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `BUILD.bazel`               | `DOLTLITE_VERSION` + shared defines, `rename_amalgamation` genrule, `sqlite3` cc_library, `doltlite` CLI cc_binary, `cli_version_test`. |
| `amalgamation.BUILD`        | BUILD file injected into the `@doltlite_amalgamation//` external repo. |
| `autoconf.BUILD`            | BUILD file injected into the `@doltlite_autoconf//` external repo; exports `shell.c`. |
| `cli_version_test.sh`       | Smoke-tests the built CLI: that it links, runs, and has a real dolt-SQL surface. Its `--version` comparison is circular and catches no drift — see [Upgrading doltlite](#upgrading-doltlite). |
| `libsqlite3-sys.patch`      | Absolutize `$(BINDIR)`-derived paths inside libsqlite3-sys's build.rs. |
| `LICENSE.md`, `APACHE_LICENSE` | Upstream's notices at the pinned version; shipped in every release's `licenses/doltlite/`. |
| `README.md`                 | This file.                                                             |
