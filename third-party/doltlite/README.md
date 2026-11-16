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
       │      ▲
       │      │  deps=[":blake3_kernels"]  (BLAKE3's SIMD kernels,
       │      │  one compile each, from @blake3_c//)
       │
       ├─────────────────────────────┐
       │                             │
       │  crate.annotation(          │  deps=[":sqlite3"]
       │    crate="libsqlite3-sys",  │
       │    deps=[":sqlite3"])       ▼
       │                     //third-party/doltlite:doltlite
       │                         (cc_binary — the CLI, for tests
       │                          and hand-inspecting raw stores)
       │
       │  //third-party/doltlite:libdoltlite.{so,dylib}
       │      (cc_binary — sqlite3.c again, as a shared library; ships
       │       in the release tarball's lib/, not on the musl platforms)
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

## BLAKE3's SIMD kernels

doltlite names every chunk it stores by its BLAKE3 hash, and hashes
again on the way back in to check what it read. The amalgamation
embeds BLAKE3 1.8.5 with its dispatcher, which asks the CPU at run time
which kernel it can run, but turns every SIMD kernel off
(`BLAKE3_NO_SSE2` … `BLAKE3_USE_NEON 0`): each kernel needs its own
`-m` flags, and one file cannot carry them. doltlite's own make build
compiles them (`main.mk`); the amalgamation does not. So doltlite
hashed every chunk with the portable code, even on a mac.

That cost real time. On a 1.46 GB Lightroom catalog (28.6k images)
the ingest's writer thread spent over 40% of its busy time in the
portable code. So here:

- `amalgamation_blake3_simd.patch` (applied by `MODULE.bazel`) deletes
  the lines that turn the kernels off.
- `@blake3_c` is BLAKE3's source at the version the amalgamation
  embeds, and `:blake3_<kernel>` compiles each kernel with its own
  flags (`BLAKE3_KERNELS` in `BUILD.bazel`): SSE2, SSE4.1, AVX2 and
  AVX-512 on x86_64, NEON on aarch64. An x86 CPU without AVX2 still
  runs: the dispatcher picks the widest kernel it has.
- Every global symbol of the kernels is renamed `doltlite_<name>`
  (`BLAKE3_KERNEL_SYMBOLS`): the Rust `blake3` crate links its own
  kernels under the same names, and a static musl link refuses two.
- The reader wheels compile the same kernels with the same flags
  (`:blake3_kernel_sources`, `scripts/release/compile_reader_library.sh`).

The hashes do not change: every kernel computes the same BLAKE3.
`:blake3_kernels_test` checks that against BLAKE3's own test vectors,
that the dispatcher has a kernel to pick, and that `@blake3_c` is the
version the amalgamation embeds.

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
| 4 | `pyproject.toml` → `doltlite==…`, dolthub's PyPI wheel, behind `import doltlite` in the Python docs; then regenerate `requirements.txt` (AGENTS.md §"Python deps") |

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
3. Check that `amalgamation_blake3_simd.patch` still applies (the
   build fails if not), and the BLAKE3 the new amalgamation embeds:
   grep `BLAKE3_VERSION_STRING` in it, and move `@blake3_c` in
   `MODULE.bazel` to that version (`:blake3_kernels_test` fails until
   they agree). If the release ships the kernels itself
   ([dolthub/doltlite#4956](https://github.com/dolthub/doltlite/issues/4956)),
   drop the patch and `@blake3_c` instead.
4. Check the chunk-store format before anything else: grep
   `CHUNK_STORE_VERSION` in the old and new `doltlite.c`. A different
   number means every existing `.doltlite_db` is refused at open — a
   migration, not a bump ([`docs/dev/doltlite.md` § Versions](../../docs/dev/doltlite.md#versions-the-storage-format-and-what-each-pin-brought)).
5. Compute both sha256s:
   ```sh
   curl -fsSL <url> | shasum -a 256
   ```
6. Update `urls` + `sha256` + `strip_prefix` in **both** `MODULE.bazel`
   `http_archive`s, and the version line in the comment above them.
7. Bump the `DOLTLITE_VERSION` constant at the top of `BUILD.bazel`.
   It feeds `-DDOLTLITE_VERSION` into both the library and the CLI.
8. `bazelisk test //tools:version_pins_test
   //third-party/doltlite:cli_version_test
   //third-party/doltlite:blake3_kernels_test` — the pins agree, and the
   CLI links and its dolt-SQL surface works. Not every release
   reaches PyPI (0.50.5–0.50.7 never did); pick one that did.
9. Run what holds the engine facts:
   `//datalib/backend/doltlite_facts:doltlite_facts_test` (one test per
   single-process fact, a module per section of `doltlite.md`) and
   `//datalib/backend/etl:doltlite_two_process_test` (a writer and a
   reader in two processes), and
   `//datalib/backend/doltlite_facts:python_package_test` and
   `:python_reader_test` (the PyPI wheel at the new pin, and the shipped
   library). A PR that moves the pin also runs `reader-wheels.yml`, which
   compiles it on glibc 2.17, musl and a Mac and reads a store under each
   ([`release_steps.md`](../../docs/dev/release_steps.md) §"The reader wheels").
   Where a fact moved, fix
   [`docs/dev/doltlite.md`](../../docs/dev/doltlite.md) first, then the
   test, and add a row to its § Versions table saying what the new pin
   brought.
   A failed count assertion elsewhere may be upstream getting *more*
   right; check before calling it a regression.
10. Then `bazelisk test //...`: every store open goes through the new
   engine.

## Files in this package

| Path                        | Purpose                                                                |
|-----------------------------|------------------------------------------------------------------------|
| `BUILD.bazel`               | `DOLTLITE_VERSION` + shared defines, `rename_amalgamation` genrule, the BLAKE3 kernels, `sqlite3` cc_library, `doltlite` CLI cc_binary, `libdoltlite` shared library, `library_copts` (the flags, for the reader wheels' compile), `cli_version_test`. |
| `amalgamation.BUILD`        | BUILD file injected into the `@doltlite_amalgamation//` external repo. |
| `amalgamation_blake3_simd.patch` | Turns the amalgamation's BLAKE3 SIMD kernels back on (§"BLAKE3's SIMD kernels"). |
| `blake3.BUILD`              | BUILD file injected into the `@blake3_c//` external repo: the kernels' sources and headers, and the test vectors. |
| `blake3_kernels_test.c`     | doltlite's BLAKE3 against BLAKE3's test vectors, with a SIMD kernel to dispatch to. |
| `autoconf.BUILD`            | BUILD file injected into the `@doltlite_autoconf//` external repo; exports `shell.c`. |
| `cli_version_test.sh`       | Smoke-tests the built CLI: that it links, runs, and has a real dolt-SQL surface. Its `--version` check compares `DOLTLITE_VERSION` with itself, so pin drift is `//tools:version_pins_test`'s job. |
| `libsqlite3-sys.patch`      | Absolutize `$(BINDIR)`-derived paths inside libsqlite3-sys's build.rs. |
| `LICENSE.md`, `APACHE_LICENSE` | Upstream's notices at the pinned version; shipped in every release's `licenses/doltlite/`. |
| `README.md`                 | This file.                                                             |
