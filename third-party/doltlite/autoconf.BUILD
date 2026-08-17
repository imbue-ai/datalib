# BUILD file injected by MODULE.bazel into the @doltlite_autoconf
# external repo (the doltlite autoconf source tarball).
#
# We want exactly ONE file out of this ~38MB tarball: the pre-generated
# `shell.c`, the sqlite3-shell CLI entry point. Everything else here —
# the `src/*.c` tree, configure, the wasm build, the test suite — is
# ignored. The amalgamation zip that `//third-party/doltlite:sqlite3`
# compiles is library-only and contains no `main()`, so this is the
# only release artifact that can supply an entry point.
#
# `shell.c` lives under `ext/wasm/bld/doltlite-build/` because that is
# where upstream's wasm build materializes the generated result of
# `src/shell.c.in`. It is not wasm-specific: it is ordinary C that
# `#include`s "sqlite3.h" and links against the library, which is
# exactly what we need. (The `src/shell.c.in` template would require
# running upstream's generator, which we deliberately do not do.)

exports_files(
    ["ext/wasm/bld/doltlite-build/shell.c"],
    visibility = ["//visibility:public"],
)
