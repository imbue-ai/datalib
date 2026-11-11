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

**Not shipped yet.** No released binary links it, so
`scripts/third_party_notices.sh` does not carry its notice; the change
that links it into one adds it.
