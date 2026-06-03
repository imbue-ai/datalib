load("@rules_cc//cc:defs.bzl", "cc_library")

# BUILD file injected by MODULE.bazel into the @doltlite_amalgamation
# external repo (the doltlite autoconf source tree, v0.11.3).
#
# The repo name is "doltlite_amalgamation" for historical reasons —
# we used to pull the amalgamation zip — but the content is the
# full autoconf source tarball now. The amalgamation `sqlite3.c` is
# only useful for stock-SQLite drop-in compatibility; the doltlite
# version-control SQL surface (`dolt_commit`, `dolt_log`, …) and the
# prolly-tree storage engine live in separate .c files under `src/`
# that the amalgamation does NOT include.
#
# Per upstream's `main.mk`, a DOLTLITE_PROLLY=1 build requires the
# non-amalgamation code path: compile every `src/*.c` individually
# (minus btree.c / pager.c / wal.c / btmutex.c / backup.c which the
# prolly engine replaces), plus the prolly + doltlite + blake3 files,
# plus the renamed-shim `*_orig.c` files. The consuming
# `//third-party/doltlite:sqlite3` rule does exactly that.

# ─────────────────────────────────────────────────────────────────
# Headers + .c-as-header files (for #include from other TUs)
# ─────────────────────────────────────────────────────────────────
#
# The renamed-shim layer does `#include "btree.c"` etc. to recompile
# vanilla SQLite under an `_orig` symbol prefix, so the vanilla `.c`
# files have to be findable on the header search path. Likewise the
# Lemon-generated parser tables ship pre-built under
# `ext/wasm/bld/doltlite-build/` (the upstream build pre-generates
# them there for the wasm distribution) — we reuse those copies
# so we don't have to run the Lemon tool ourselves in bazel.

cc_library(
    name = "doltlite_headers",
    hdrs = glob([
        "src/**/*.h",
        "src/*.c",
        "ext/blake3/**/*.h",
        "ext/fts3/*.h",
        "ext/fts5/*.h",
        "ext/rtree/*.h",
        "ext/session/*.h",
        "ext/rbu/*.h",
        "ext/misc/*.h",
        "ext/wasm/bld/doltlite-build/*.h",
        "ext/wasm/bld/doltlite-build/parse.c",
        "ext/wasm/bld/doltlite-build/opcodes.c",
        # `rtree.c` uses `#include "geopoly.c"` rather than compiling
        # it separately; expose the .c as a header.
        "ext/rtree/geopoly.c",
    ]),
    includes = [
        ".",
        "src",
        "ext/blake3",
        "ext/fts3",
        "ext/fts5",
        "ext/rtree",
        "ext/session",
        "ext/rbu",
        "ext/misc",
        "ext/wasm/bld/doltlite-build",
    ],
    visibility = ["//visibility:public"],
)

# ─────────────────────────────────────────────────────────────────
# Source filegroups (translation units that get compiled)
# ─────────────────────────────────────────────────────────────────

# SQLite core sources. Mirrors LIBOBJS0 from upstream's main.mk,
# minus the five files the prolly engine replaces (btree.c, pager.c,
# wal.c, btmutex.c, backup.c) and all `test*.c` (Tcl-only test
# harness code).
filegroup(
    name = "sqlite_core_src",
    srcs = glob(
        ["src/*.c"],
        exclude = [
            # Replaced by the prolly storage engine
            "src/btree.c",
            "src/pager.c",
            "src/wal.c",
            "src/btmutex.c",
            "src/backup.c",
            # Test harness — not part of the library
            "src/test*.c",
            "src/tclsqlite.c",
            # Doltlite + prolly engine files — listed separately below
            "src/doltlite*.c",
            "src/prolly_*.c",
            "src/chunk_*.c",
            "src/pager_shim.c",
            "src/sortkey.c",
            "src/btree_orig.c",
            "src/pager_orig.c",
            "src/wal_orig.c",
            "src/btmutex_orig.c",
            "src/backup_orig.c",
            "src/btree_orig_api.c",
            # Windows-only mutex backend
            "src/mutex_w32.c",
            # Windows-only OS adapter
            "src/os_win.c",
            # Shell (not part of the library proper)
            "src/shell.c",
        ],
    ),
    visibility = ["//visibility:public"],
)

# Prolly-tree storage engine + doltlite SQL surface + renamed
# original-btree shim. Exactly the set PROLLY_OBJS pulls in.
filegroup(
    name = "doltlite_engine_src",
    srcs = glob([
        "src/doltlite*.c",
        "src/prolly_*.c",
        "src/chunk_*.c",
        "src/pager_shim.c",
        "src/sortkey.c",
        "src/btree_orig.c",
        "src/pager_orig.c",
        "src/wal_orig.c",
        "src/btmutex_orig.c",
        "src/backup_orig.c",
        "src/btree_orig_api.c",
    ]),
    visibility = ["//visibility:public"],
)

# SQLite extensions doltlite compiles into the library by default:
# FTS3/4, FTS5, RTree, ICU, session, RBU, the `stmt` vtable.
# FTS5 ships as a pre-built amalgamation `fts5.c` under the wasm
# build directory rather than as individual `ext/fts5/*.c` files.
filegroup(
    name = "sqlite_extension_src",
    srcs = [
        "ext/wasm/bld/doltlite-build/fts5.c",
        # ICU intentionally NOT compiled — needs system-installed
        # libicu headers (`unicode/ucol.h`), which we don't want as a
        # host dependency. FTS3 has its own tokenizer; ICU integration
        # only matters for the FTS3 `icu` tokenizer, which nothing in
        # this tree uses.
        "ext/rtree/rtree.c",
        "ext/session/sqlite3session.c",
        "ext/rbu/sqlite3rbu.c",
        "ext/misc/stmt.c",
    ] + glob(
        ["ext/fts3/fts3*.c"],
        exclude = [
            # `fts3_icu.c` is the ICU-specific tokenizer; skip it for
            # the same reason we skip `ext/icu/icu.c`.
            "ext/fts3/fts3_icu.c",
            # `fts3_test.c` is a TCL-based test harness, not part of
            # the runtime library. It #includes `src/tclsqlite.h`,
            # which pulls in `<tcl.h>`. macOS Command Line Tools ship
            # `tcl.h`, but Ubuntu runners don't (and we don't want
            # tcl-dev as a build dep). Drop it from the library link.
            "ext/fts3/fts3_test.c",
        ],
    ),
    visibility = ["//visibility:public"],
)

# Lemon-generated parser tables + the `ctime.c` / `tclsqlite-ex.c`
# special files. Pre-built by upstream for the wasm distribution —
# parser output is platform-independent (Lemon takes parse.y and
# emits portable C), so we can reuse the wasm-build artifacts.
filegroup(
    name = "sqlite_generated_src",
    srcs = [
        "ext/wasm/bld/doltlite-build/ctime.c",
        "ext/wasm/bld/doltlite-build/opcodes.c",
        "ext/wasm/bld/doltlite-build/parse.c",
    ],
    visibility = ["//visibility:public"],
)

# Per-platform SIMD acceleration for blake3 (the prolly engine's
# content-addressed hash). The portable + dispatch .c files are
# always compiled; whichever SIMD variants are appropriate are
# added by a select() in the consuming cc_library.
filegroup(
    name = "blake3_portable_src",
    srcs = [
        "ext/blake3/blake3.c",
        "ext/blake3/blake3_dispatch.c",
        "ext/blake3/blake3_portable.c",
    ],
    visibility = ["//visibility:public"],
)

filegroup(
    name = "blake3_x86_simd_src",
    srcs = [
        "ext/blake3/blake3_avx2.c",
        "ext/blake3/blake3_avx512.c",
        "ext/blake3/blake3_sse2.c",
        "ext/blake3/blake3_sse41.c",
    ],
    visibility = ["//visibility:public"],
)

filegroup(
    name = "blake3_arm_simd_src",
    srcs = ["ext/blake3/blake3_neon.c"],
    visibility = ["//visibility:public"],
)

# Public header. Consumers (Rust crates like libsqlite3-sys) read
# `#include <sqlite3.h>` and they want the exact file the doltlite
# build produces — which is identical to upstream's `sqlite3.h`
# since doltlite preserves the public API byte-for-byte.
exports_files(
    [
        "sqlite3.h",
        "sqlite3ext.h",
    ],
    visibility = ["//visibility:public"],
)
