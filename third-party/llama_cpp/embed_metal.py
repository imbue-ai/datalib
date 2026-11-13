"""ggml's Metal shaders as an assembly file that puts their source in the binary.

What ggml/src/ggml-metal/CMakeLists.txt does with GGML_METAL_EMBED_LIBRARY:
splice ggml-common.h and ggml-metal-impl.h into ggml-metal.metal, and define
`ggml_metallib_start` and `ggml_metallib_end` around the result, which
ggml-metal-device.m hands to Metal to compile when the device opens. The
bytes are written out with `.byte` rather than `.incbin`, so the assembler
needs no path into the build tree.

usage: embed_metal.py <ggml-metal.metal> <ggml-common.h> <ggml-metal-impl.h> <out.s>
"""

import sys


def main() -> None:
    metal_path, common_path, impl_path, out_path = sys.argv[1:]
    common = open(common_path, encoding="utf-8").read()
    impl = open(impl_path, encoding="utf-8").read()
    lines = []
    for line in open(metal_path, encoding="utf-8").read().splitlines(keepends=True):
        if "__embed_ggml-common.h__" in line:
            lines.append(common)
        elif line.strip() == '#include "ggml-metal-impl.h"':
            lines.append(impl)
        else:
            lines.append(line)
    data = "".join(lines).encode("utf-8")
    with open(out_path, "w", encoding="ascii") as out:
        out.write(".section __DATA,__ggml_metallib\n")
        out.write(".globl _ggml_metallib_start\n_ggml_metallib_start:\n")
        for i in range(0, len(data), 64):
            out.write(".byte " + ",".join(str(b) for b in data[i : i + 64]) + "\n")
        out.write(".globl _ggml_metallib_end\n_ggml_metallib_end:\n")


if __name__ == "__main__":
    main()
