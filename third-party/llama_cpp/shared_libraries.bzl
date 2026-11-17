"""The shared libraries a C++ target links against, as files.

The `-cuda` image ships the cuBLAS that `libggml-cuda.so` links, and
this hands over those exact files: the ones rules_cuda imports from
NVIDIA's sha256-pinned archive (`@cuda//:cublas`), not a copy from
somewhere else.
"""

load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")

def _shared_libraries_impl(ctx):
    libs = []
    for dep in ctx.attr.deps:
        for linker_input in dep[CcInfo].linking_context.linker_inputs.to_list():
            for lib in linker_input.libraries:
                if lib.dynamic_library:
                    libs.append(lib.dynamic_library)
    return [DefaultInfo(files = depset(libs))]

shared_libraries = rule(
    implementation = _shared_libraries_impl,
    attrs = {"deps": attr.label_list(providers = [CcInfo])},
)
