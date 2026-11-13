# BUILD file injected by MODULE.bazel into the @llama_cpp external repo.
#
# ggml with its CPU backend only, and libllama over it: what the search
# index's embedder needs to run one GGUF embedding model. The source lists
# follow ggml/src/CMakeLists.txt, ggml/src/ggml-cpu/CMakeLists.txt and
# src/CMakeLists.txt at the pinned release; a bump re-reads them.
# On macOS, Metal too, with its shader source embedded in the binary; on
# Linux x86_64, CUDA as a module built apart (at the end of this file).
# Left out: every other GPU backend, OpenMP, Accelerate and KleidiAI (a
# second download), and the tools under common/ and tools/.

load("@rules_cc//cc:cc_library.bzl", "cc_library")
load("@rules_cc//cc:cc_shared_library.bzl", "cc_shared_library")
load("@rules_cuda//cuda:defs.bzl", "cuda_library")

GGML_VERSION = "0.19.0"

LLAMA_RELEASE = "b10361"

# The fully-static release builds (//platforms:musl).
config_setting(
    name = "musl",
    constraint_values = [
        "@@//platforms:musl",
        "@platforms//os:linux",
    ],
)

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

# ggml-metal's Objective-C, compiled as such through cc_library, which does
# not take a `.m`: each file is copied under a `.c` name and built with
# `-x objective-c`, in a library of its own so the flag reaches nothing else.
METAL_OBJC = [
    "ggml-metal-device",
    "ggml-metal-context",
]

[
    genrule(
        name = "copy_" + name,
        srcs = ["ggml/src/ggml-metal/%s.m" % name],
        outs = ["ggml/src/ggml-metal/%s_objc.c" % name],
        cmd = "cp $< $@",
    )
    for name in METAL_OBJC
]

genrule(
    name = "metal_embed",
    srcs = [
        "ggml/src/ggml-metal/ggml-metal.metal",
        "ggml/src/ggml-common.h",
        "ggml/src/ggml-metal/ggml-metal-impl.h",
    ],
    outs = ["ggml/src/ggml-metal/ggml-metal-embed.s"],
    cmd = "python3 $(execpath @@//third-party/llama_cpp:embed_metal.py) $(SRCS) $@",
    tools = ["@@//third-party/llama_cpp:embed_metal.py"],
)

cc_library(
    name = "ggml_headers",
    hdrs = glob([
        "ggml/include/*.h",
        "ggml/src/*.h",
        "ggml/src/ggml-metal/*.h",
    ]),
    includes = [
        "ggml/include",
        "ggml/src",
        "ggml/src/ggml-metal",
    ],
)

cc_library(
    name = "ggml_metal_objc",
    srcs = [":copy_" + name for name in METAL_OBJC],
    copts = COMMON_COPTS + [
        "-x",
        "objective-c",
        "-DGGML_METAL_EMBED_LIBRARY",
    ],
    target_compatible_with = ["@platforms//os:macos"],
    deps = [":ggml_headers"],
)

METAL_SRCS = select({
    "@platforms//os:macos": [
        "ggml/src/ggml-metal/ggml-metal.cpp",
        "ggml/src/ggml-metal/ggml-metal-device.cpp",
        "ggml/src/ggml-metal/ggml-metal-common.cpp",
        "ggml/src/ggml-metal/ggml-metal-ops.cpp",
        ":metal_embed",
    ],
    "//conditions:default": [],
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
    ] + ARCH_SRCS + METAL_SRCS + glob([
        "ggml/src/*.h",
        "ggml/src/ggml-cpu/**/*.h",
        "ggml/src/ggml-metal/*.h",
    ]),
    hdrs = glob(["ggml/include/*.h"]),
    conlyopts = ["-std=gnu11"],
    copts = COMMON_COPTS + [
        "-DGGML_VERSION=\\\"" + GGML_VERSION + "\\\"",
        "-DGGML_COMMIT=\\\"" + LLAMA_RELEASE + "\\\"",
        "-DGGML_USE_LLAMAFILE",
        "-DGGML_USE_CPU_REPACK",
    ] + select({
        "@platforms//os:macos": ["-DGGML_METAL_EMBED_LIBRARY"],
        "//conditions:default": [],
    }),
    cxxopts = ["-std=c++17"],
    defines = ["GGML_USE_CPU"] + select({
        "@platforms//os:macos": ["GGML_USE_METAL"],
        "//conditions:default": [],
    }),
    includes = [
        "ggml/include",
        "ggml/src",
        "ggml/src/ggml-cpu",
    ],
    # rustc drives the final link. The host toolchains add the C++ runtime
    # to it; the musl one does not, and its link is static, takes no
    # default libraries, and ends in -Bdynamic, where a bare -l finds a
    # .so. So name the archives, after the C++ runtime what it calls:
    # libc, and libgcc's helpers (aarch64's outline atomics among them).
    # The glibc link needs libgcc.a named too: rustc puts -lgcc_s before
    # its last -lllama.pic, so with gcc 13 (ubuntu-24.04-arm) llama's
    # `__aarch64_ldset4_relax` and its kin are wanted after libgcc was
    # scanned, and the link fails.
    linkopts = select({
        ":musl": [
            "-l:libstdc++.a",
            "-l:libc.a",
            "-l:libgcc.a",
        ],
        "@platforms//os:linux": [
            "-lpthread",
            "-ldl",
            "-lm",
            "-l:libgcc.a",
            # For libggml-cuda.so, which takes ggml from the program
            # that loads it (below). A list, not --export-dynamic-symbol:
            # gold, which links the gnu builds, takes no glob there.
            "-Wl,--dynamic-list=$(execpath @@//third-party/llama_cpp:ggml_exports.dynlist)",
        ],
        "@platforms//os:macos": [
            "-framework",
            "Foundation",
            "-framework",
            "Metal",
            "-framework",
            "MetalKit",
        ],
        "//conditions:default": [],
    }),
    additional_linker_inputs = ["@@//third-party/llama_cpp:ggml_exports.dynlist"],
    visibility = ["//visibility:public"],
    # Whole, so a part of ggml that only libggml-cuda.so calls is still
    # there to export.
    alwayslink = True,
    deps = select({
        "@platforms//os:macos": [":ggml_metal_objc"],
        "//conditions:default": [],
    }),
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

# The CUDA backend as a module, libggml-cuda.so, that a gnu build loads
# with ggml_backend_load when there is a GPU
# (docs/dev/plans/embed_on_modal.md). Upstream links such a module against
# a shared libggml-base.so; ours leaves ggml's symbols undefined and takes
# them from the program that loads it, whose ggml is linked in statically
# and exported (`ggml`'s linkopts above), so the process holds one ggml
# and the binaries stay self-contained. The module needs the host's
# libcuda.so.1 and cuBLAS (libcublas.so.12, libcublasLt.so.12); the CUDA
# runtime is linked in. Linux x86_64 only.

LINUX_X86_64 = [
    "@platforms//os:linux",
    "@platforms//cpu:x86_64",
]

# The four fattn-vec instances upstream builds without GGML_CUDA_FA_ALL_QUANTS.
GGML_CUDA_SRCS = glob(
    [
        "ggml/src/ggml-cuda/*.cu",
        "ggml/src/ggml-cuda/template-instances/fattn-tile*.cu",
        "ggml/src/ggml-cuda/template-instances/fattn-mma*.cu",
        "ggml/src/ggml-cuda/template-instances/mmq*.cu",
        "ggml/src/ggml-cuda/template-instances/mmf*.cu",
    ],
) + [
    "ggml/src/ggml-cuda/template-instances/fattn-vec-instance-f16-f16.cu",
    "ggml/src/ggml-cuda/template-instances/fattn-vec-instance-q4_0-q4_0.cu",
    "ggml/src/ggml-cuda/template-instances/fattn-vec-instance-q8_0-q8_0.cu",
    "ggml/src/ggml-cuda/template-instances/fattn-vec-instance-bf16-bf16.cu",
]

cuda_library(
    name = "ggml_cuda_objs",
    srcs = GGML_CUDA_SRCS,
    hdrs = glob([
        "ggml/src/ggml-cuda/**/*.cuh",
        "ggml/src/ggml-cuda/vendors/*.h",
        "ggml/src/*.h",
        "ggml/include/*.h",
    ]),
    # The GPUs Modal rents: T4 (7.5), A100 (8.0), A10G (8.6), L4 and L40S
    # (8.9), H100 and H200 (9.0); 9.0's PTX too, which a newer GPU (B200)
    # compiles when it loads the module.
    archs = [
        "compute_75:sm_75",
        "compute_80:sm_80,sm_86,sm_89",
        "compute_90:compute_90,sm_90",
    ],
    copts = [
        "-use_fast_math",
        "-extended-lambda",
        "-compress-mode=size",
    ],
    host_copts = [
        "-O3",
        "-fPIC",
        "-w",
    ],
    includes = [
        "ggml/include",
        "ggml/src",
        "ggml/src/ggml-cuda",
    ],
    local_defines = [
        "NDEBUG",
        "GGML_BACKEND_DL",
        "GGML_BACKEND_BUILD",
        "GGML_BACKEND_SHARED",
        "GGML_SHARED",
        "GGML_CUDA_PEER_MAX_BATCH_SIZE=128",
        "GGML_CUDA_USE_GRAPHS",
    ],
    target_compatible_with = LINUX_X86_64,
    alwayslink = True,
    # cuBLAS is linked as the host's libcublas.so.12: static, it was most of
    # a 939 MB module. Anyone running CUDA has the toolkit's libraries.
    deps = [
        "@cuda//:cublas",
        "@cuda//:cuda",
    ],
)

cc_shared_library(
    name = "ggml_cuda_so",
    shared_lib_name = "libggml-cuda.so",
    target_compatible_with = LINUX_X86_64,
    visibility = ["//visibility:public"],
    deps = [":ggml_cuda_objs"],
)

exports_files([
    "LICENSE",
    "ggml/src/ggml-cpu/llamafile/sgemm.cpp",
])
