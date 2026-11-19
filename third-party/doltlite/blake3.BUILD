# BUILD file injected by MODULE.bazel into the @blake3_c external repo
# (BLAKE3's source at the version doltlite embeds). Only
# `//third-party/doltlite:blake3_kernels_test` takes it: the header that
# declares BLAKE3's functions, and the test vectors it checks doltlite's
# hashing against. Doltlite's amalgamation carries all of BLAKE3 itself.

load("@rules_cc//cc:defs.bzl", "cc_library")

exports_files(
    ["test_vectors/test_vectors.json"],
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
