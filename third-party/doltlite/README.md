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
       │  bazel_dep + http_archive(name="doltlite_amalgamation",
       │                           sha256="…")
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
       │  crate.annotation(crate="libsqlite3-sys",
       │                   deps=[":sqlite3"])
       ▼
   @frankweiler_crates//:libsqlite3-sys
   @frankweiler_crates//:sqlx-sqlite
   …all the way up to the binaries.
```

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

1. Find the new release: <https://github.com/dolthub/doltlite/releases>.
2. Pick the **amalgamation** zip (e.g.
   `doltlite-amalgamation-X.Y.Z.zip`). **Do not use any 0.11.x release
   before 0.11.4** — those amalgamation zips were broken and built
   stock SQLite, missing the prolly hooks.
3. Compute the sha256:
   ```sh
   curl -fsSL <url> | shasum -a 256
   ```
4. Update `urls` + `sha256` + `strip_prefix` in `MODULE.bazel`'s
   `http_archive(name = "doltlite_amalgamation", ...)`.
5. Bump `DOLTLITE_VERSION` in `BUILD.bazel`'s `copts`.
6. `bazelisk build //...` — Bazel re-downloads, recompiles, and feeds
   the new archive into every downstream binary.

No code or wiring changes needed unless the doltlite public API shifts
(it's a SQLite fork, so it shouldn't).

## Files in this package

| Path                        | Purpose                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `BUILD.bazel`               | `rename_amalgamation` genrule + `sqlite3` cc_library.                  |
| `amalgamation.BUILD`        | BUILD file injected into the `@doltlite_amalgamation//` external repo. |
| `libsqlite3-sys.patch`      | Absolutize `$(BINDIR)`-derived paths inside libsqlite3-sys's build.rs. |
| `README.md`                 | This file.                                                             |
