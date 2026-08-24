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

A version lives in **four** places and they must all move together:

| # | Location |
|---|----------|
| 1 | `MODULE.bazel` → `http_archive(name = "doltlite_amalgamation")` — the library |
| 2 | `MODULE.bazel` → `http_archive(name = "doltlite_autoconf")` — the CLI's `shell.c` |
| 3 | `BUILD.bazel` → `DOLTLITE_VERSION` |
| 4 | `datalib/docker/Dockerfile` → `DOLTLITE_CLI_VERSION` — the container's debug-shell `.deb` |

**Nothing mechanically verifies that these four agree — check them by
hand.** `:cli_version_test` reads like it does this, and its comments
say so, but the check is circular: the CLI prints the version it was
compiled with (`-DDOLTLITE_VERSION`, from #3), and the test compares
that against #3 again. Setting `DOLTLITE_VERSION = "0.11.52"` while
both archives are on 0.11.53 passes. What the test *does* genuinely
catch is worth keeping — that the CLI links and runs at all, and that
its dolt-SQL surface is real (it exercises `dolt_commit` and
`dolt_log`, so a shell accidentally linked against stock SQLite fails).
It just isn't a pin-drift guard. Pin #4 has drifted before, sitting at
0.11.8 while the library was on 0.11.13.

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
6. Bump `DOLTLITE_CLI_VERSION` in `datalib/docker/Dockerfile`: a `.deb`
   from the same upstream release, pinned to the linked library on
   purpose so the SQL surface in the container's debug shell matches
   what the binary observes.
7. Re-grep to confirm all four moved — this is the only thing standing
   between you and a silent mismatch:
   ```sh
   grep -rn '0\.11\.' MODULE.bazel third-party/doltlite/BUILD.bazel \
       datalib/docker/Dockerfile
   ```
8. `bazelisk test //third-party/doltlite:cli_version_test` — confirms
   the CLI links and its dolt-SQL surface works against the new
   engine. Then `bazelisk build //...` for everything downstream.

Before bumping, check whether the chunk-store format moved: grep
`CHUNK_STORE_VERSION` in the old and new `doltlite.c`. The open path
hard-rejects any mismatch (`SQLITE_NOTADB`, "written by an incompatible
doltlite version") with no migration path, so a bump there orphans every
existing `.doltlite_db` on disk rather than merely needing a rebuild.
It has been `12` from 0.11.13 through 0.11.53; 0.11.40 froze 12 as the
beta compatibility boundary.

Also worth a moment: a bump can move the *SQL surface's* semantics
without touching the storage format, and the tests that notice are the
ones asserting exact counts. 0.11.52 changed `dolt_diff.data_change` to
report `0` for a newly created **empty** table (it had been `1` through
0.11.51) — correct, but it moved a lightroom assertion from 113 tables
to the 38 that actually hold rows. If a bump fails a count assertion,
check whether upstream got *more* right before assuming a regression.

No code or wiring changes needed unless the doltlite public API shifts
(it's a SQLite fork, so it shouldn't).

## Files in this package

| Path                        | Purpose                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `BUILD.bazel`               | `DOLTLITE_VERSION` + shared defines, `rename_amalgamation` genrule, `sqlite3` cc_library, `doltlite` CLI cc_binary, `cli_version_test`. |
| `amalgamation.BUILD`        | BUILD file injected into the `@doltlite_amalgamation//` external repo. |
| `autoconf.BUILD`            | BUILD file injected into the `@doltlite_autoconf//` external repo; exports `shell.c`. |
| `cli_version_test.sh`       | Smoke-tests the built CLI: that it links, runs, and has a real dolt-SQL surface. Its `--version` comparison is circular and catches no drift — see [Upgrading doltlite](#upgrading-doltlite). |
| `libsqlite3-sys.patch`      | Absolutize `$(BINDIR)`-derived paths inside libsqlite3-sys's build.rs. |
| `README.md`                 | This file.                                                             |
