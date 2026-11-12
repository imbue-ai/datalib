# llama.cpp

[llama.cpp](https://github.com/ggml-org/llama.cpp) (MIT, its `LICENSE` in
the archive) at release `b10361`, fetched by `MODULE.bazel` as
`@llama_cpp` and built by [`llama_cpp.BUILD`](llama_cpp.BUILD): ggml with
its CPU backend, and on macOS its Metal backend, and libllama over it.
Metal's shader source is put in the binary by
[`embed_metal.py`](embed_metal.py), as llama.cpp's own
`GGML_METAL_EMBED_LIBRARY` does, and Metal compiles it when the device
opens, so the build needs no Metal compiler. The search index's embedder
(`datalib/backend/embedder`) is the one thing that links it.

**Why this release.** It is the one node-llama-cpp 3.20.0 is built on
(its `llama/binariesGithubRelease.json`), and node-llama-cpp 3.20.0 is
what qmd 2.8.3 runs. Running the same release gives the same tokenizer,
graph and kernels as the vectors qmd wrote, which
`//datalib/backend/embedder:parity_test` checks against qmd itself.

**Bumping it.** Change the tag, URL and `sha256` in `MODULE.bazel`, then
re-read `ggml/src/CMakeLists.txt`, `ggml/src/ggml-cpu/CMakeLists.txt`
and `src/CMakeLists.txt` at the new tag for added or removed sources and
definitions, and run the embedder's tests. Moving away from qmd's
release means the parity test compares against a different engine, so
say in the change what moved.

**The static Linux builds.** The musl toolchain adds no C++ runtime to
the link rustc drives, so `llama_cpp.BUILD` names `libstdc++.a`, `libc.a`
and `libgcc.a` itself. CI links only x86_64 on a PR; after changing the
link, cross-build `//datalib/backend/datalib_step` from a mac with
`--config=musl-aarch64` too, where libgcc's outline atomics are needed.

**Shipped in `datalib-step`.** `//third-party:bundled_licenses` carries
its license, and the header of llamafile's `sgemm.cpp` (Mozilla's
copyright), into a release's `licenses/llama.cpp/`.
