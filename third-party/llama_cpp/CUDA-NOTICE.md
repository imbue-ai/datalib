# NVIDIA CUDA components

These files are NVIDIA's, not datalib's, and datalib's MIT license does
not cover them. They are redistributed unmodified under the NVIDIA CUDA
Toolkit End User License Agreement
(<https://docs.nvidia.com/cuda/eula/index.html>), which lists them in its
Attachment A as files that may be distributed with an application:

- **The CUDA Runtime** (`libcudart_static.a`), linked into
  `libggml-cuda.so`.
- **cuBLAS** (`libcublas.so.12`, `libcublasLt.so.12`), in the
  `ghcr.io/imbue-ai/datalib:<version>-cuda` image only.

Both come from CUDA Toolkit 12.9, from the archives NVIDIA publishes for
redistribution, pinned by sha256 in datalib's `MODULE.bazel`. They are
there for datalib's embedder to run on an NVIDIA GPU, and for nothing
else. NVIDIA's driver library (`libcuda.so.1`) is not included: the
machine with the GPU provides it.

`libggml-cuda.so` itself is ggml's CUDA backend, from llama.cpp (MIT);
its license is `licenses/llama.cpp/LICENSE`.
