# doltlite (bazel-vendored)

This directory wires **doltlite** — a SQLite fork with content-addressed
prolly-tree storage and `dolt_commit()` / `dolt_log()` SQL functions —
into the Rust build as a statically-linked dependency. After the build,
every binary that touches sqlx-sqlite ships doltlite inside itself; no
runtime `brew install`, no system libsqlite3 dependency.

This page is about the vendoring. What the engine does, and what each
pin brought, is [`docs/dev/doltlite.md`](../../docs/dev/doltlite.md).

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

The two `http_archive`s must name the same doltlite release;
`//tools:version_pins_test` checks that they and `DOLTLITE_VERSION`
agree.

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

Steps:

1. Find the new release: <https://github.com/dolthub/doltlite/releases>.
2. You need **two** assets, at the same version:
   - `doltlite-amalgamation-X.Y.Z.zip` — the library (`doltlite.{c,h}`).
   - `doltlite-autoconf-X.Y.Z.tar.gz` — for its pre-generated `shell.c`
     only. The amalgamation is library-only (no `main()`), so the CLI
     has to come from here.
3. Check the chunk-store format before anything else: grep
   `CHUNK_STORE_VERSION` in the old and new `doltlite.c`. A different
   number means every existing `.doltlite_db` is refused at open — a
   migration, not a bump ([`docs/dev/doltlite.md` § Versions](../../docs/dev/doltlite.md#versions-the-storage-format-and-what-each-pin-brought)).
4. Compute both sha256s:
   ```sh
   curl -fsSL <url> | shasum -a 256
   ```
5. Update `urls` + `sha256` + `strip_prefix` in **both** `MODULE.bazel`
   `http_archive`s, and the version line in the comment above them.
6. Bump the `DOLTLITE_VERSION` constant at the top of `BUILD.bazel`.
   It feeds `-DDOLTLITE_VERSION` into both the library and the CLI.
7. `bazelisk test //tools:version_pins_test
   //third-party/doltlite:cli_version_test` — the pins agree, and the
   CLI links and its dolt-SQL surface works.
8. Run what holds the engine facts:
   `//datalib/backend/doltlite_facts:doltlite_facts_test` (one test per
   single-process fact, a module per section of `doltlite.md`) and
   `//datalib/backend/etl:doltlite_two_process_test` (a writer and a
   reader in two processes). Where a fact moved, fix
   [`docs/dev/doltlite.md`](../../docs/dev/doltlite.md) first, then the
   test, and add a row to its § Versions table saying what the new pin
   brought.
   A failed count assertion elsewhere may be upstream getting *more*
   right; check before calling it a regression.
9. Then `bazelisk test //...`: every store open goes through the new
   engine.

## Files in this package

| Path                        | Purpose                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `BUILD.bazel`               | `DOLTLITE_VERSION` + shared defines, `rename_amalgamation` genrule, `sqlite3` cc_library, `doltlite` CLI cc_binary, `cli_version_test`. |
| `amalgamation.BUILD`        | BUILD file injected into the `@doltlite_amalgamation//` external repo. |
| `autoconf.BUILD`            | BUILD file injected into the `@doltlite_autoconf//` external repo; exports `shell.c`. |
| `cli_version_test.sh`       | Smoke-tests the built CLI: that it links, runs, and has a real dolt-SQL surface. Its `--version` check compares `DOLTLITE_VERSION` with itself, so pin drift is `//tools:version_pins_test`'s job. |
| `libsqlite3-sys.patch`      | Absolutize `$(BINDIR)`-derived paths inside libsqlite3-sys's build.rs. |
| `LICENSE.md`, `APACHE_LICENSE` | Upstream's notices at the pinned version; shipped in every release's `licenses/doltlite/`. |
| `README.md`                 | This file.                                                             |
