# BUILD file injected by MODULE.bazel into the @doltlite_amalgamation
# external repo (the doltlite amalgamation zip, v0.11.4+).
#
# We take two files out of the zip: `doltlite.c` (the single-TU
# amalgamation with SQLite core + prolly engine + dolt-SQL surface +
# embedded blake3) and `doltlite.h` (the public header). It also ships
# `doltliteext.h`, which neither of those two `#include`s and nothing
# here compiles; leaving it unexported is deliberate. The consuming
# `//third-party/doltlite:sqlite3` rule renames them to the canonical
# `sqlite3.{c,h}` names that `libsqlite3-sys` looks for, then compiles
# `sqlite3.c` into `libsqlite3.a`.

exports_files(
    [
        "doltlite.c",
        "doltlite.h",
    ],
    visibility = ["//visibility:public"],
)
