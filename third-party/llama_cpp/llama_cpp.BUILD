# BUILD file injected by MODULE.bazel into the @llama_cpp external repo.
#
# ggml with its CPU backend only, and libllama over it: what the search
# index's embedder needs to run one GGUF embedding model. The source lists
# follow ggml/src/CMakeLists.txt, ggml/src/ggml-cpu/CMakeLists.txt and
# src/CMakeLists.txt at the pinned release; a bump re-reads them.
# Left out: every GPU backend, OpenMP, Accelerate and KleidiAI (a second
# download), and the tools under common/ and tools/.

load("@rules_cc//cc:cc_library.bzl", "cc_library")

GGML_VERSION = "0.19.0"

LLAMA_RELEASE = "b10361"

COMMON_COPTS = [
    # Numeric loops; at fastbuild's -O0 an embedding takes many times as long.
    "-O3",
    "-DNDEBUG",
    # Linked into Rust binaries, which link as PIE on Linux.
    "-fPIC",
    "-w",
] + select({
    "@platforms//os:macos": ["-D_DARWIN_C_SOURCE"],
    "@platforms//os:linux": ["-D_GNU_SOURCE"],
    "//conditions:default": [],
}) + ["-D_XOPEN_SOURCE=600"]

ARCH_SRCS = select({
    "@platforms//cpu:aarch64": [
        "ggml/src/ggml-cpu/arch/arm/quants.c",
        "ggml/src/ggml-cpu/arch/arm/repack.cpp",
    ],
    "@platforms//cpu:x86_64": [
        "ggml/src/ggml-cpu/arch/x86/quants.c",
        "ggml/src/ggml-cpu/arch/x86/repack.cpp",
    ],
})

cc_library(
    name = "ggml",
    srcs = [
        "ggml/src/ggml.c",
        "ggml/src/ggml.cpp",
        "ggml/src/ggml-alloc.c",
        "ggml/src/ggml-backend.cpp",
        "ggml/src/ggml-backend-meta.cpp",
        "ggml/src/ggml-opt.cpp",
        "ggml/src/ggml-threading.cpp",
        "ggml/src/ggml-quants.c",
        "ggml/src/gguf.cpp",
        "ggml/src/ggml-backend-dl.cpp",
        "ggml/src/ggml-backend-reg.cpp",
        "ggml/src/ggml-cpu/ggml-cpu.c",
        "ggml/src/ggml-cpu/ggml-cpu.cpp",
        "ggml/src/ggml-cpu/repack.cpp",
        "ggml/src/ggml-cpu/hbm.cpp",
        "ggml/src/ggml-cpu/quants.c",
        "ggml/src/ggml-cpu/traits.cpp",
        "ggml/src/ggml-cpu/amx/amx.cpp",
        "ggml/src/ggml-cpu/amx/mmq.cpp",
        "ggml/src/ggml-cpu/binary-ops.cpp",
        "ggml/src/ggml-cpu/unary-ops.cpp",
        "ggml/src/ggml-cpu/vec.cpp",
        "ggml/src/ggml-cpu/ops.cpp",
        "ggml/src/ggml-cpu/llamafile/sgemm.cpp",
    ] + ARCH_SRCS + glob([
        "ggml/src/*.h",
        "ggml/src/ggml-cpu/**/*.h",
    ]),
    hdrs = glob(["ggml/include/*.h"]),
    conlyopts = ["-std=gnu11"],
    copts = COMMON_COPTS + [
        "-DGGML_VERSION=\\\"" + GGML_VERSION + "\\\"",
        "-DGGML_COMMIT=\\\"" + LLAMA_RELEASE + "\\\"",
        "-DGGML_USE_LLAMAFILE",
        "-DGGML_USE_CPU_REPACK",
    ],
    cxxopts = ["-std=c++17"],
    defines = ["GGML_USE_CPU"],
    includes = [
        "ggml/include",
        "ggml/src",
        "ggml/src/ggml-cpu",
    ],
    linkopts = select({
        "@platforms//os:linux": [
            "-lpthread",
            "-ldl",
            "-lm",
        ],
        "//conditions:default": [],
    }),
    visibility = ["//visibility:public"],
)

cc_library(
    name = "llama",
    srcs = glob([
        "src/*.cpp",
        "src/*.h",
        "src/models/*.cpp",
        "src/models/*.h",
    ]),
    hdrs = ["include/llama.h"],
    copts = COMMON_COPTS,
    cxxopts = ["-std=c++17"],
    includes = [
        "include",
        "src",
    ],
    visibility = ["//visibility:public"],
    deps = [":ggml"],
)

exports_files(["LICENSE"])
