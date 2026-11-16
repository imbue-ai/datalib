# BUILD file injected by MODULE.bazel into the @blake3_c external repo
# (BLAKE3's source at the version doltlite embeds). We take its C SIMD
# kernels and the headers they include; doltlite's amalgamation has the
# rest of BLAKE3 already. `//third-party/doltlite:blake3_kernels`
# compiles them, and the test vectors check what they compute.

load("@rules_cc//cc:defs.bzl", "cc_library")

exports_files(
    glob(["c/blake3_*.c"]) + ["test_vectors/test_vectors.json"],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "headers",
    hdrs = [
        "c/blake3.h",
        "c/blake3_impl.h",
    ],
    strip_include_prefix = "c",
    visibility = ["//visibility:public"],
)

filegroup(
    name = "header_files",
    srcs = [
        "c/blake3.h",
        "c/blake3_impl.h",
    ],
    visibility = ["//visibility:public"],
)
