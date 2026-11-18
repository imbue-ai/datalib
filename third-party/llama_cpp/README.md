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

**One patch.** [`ggml_barrier_sleeps.patch`](ggml_barrier_sleeps.patch)
changes `ggml_barrier`, where the CPU backend's threads meet hundreds of
times per forward pass. Upstream waits there in a pure busy loop, so on
a machine with more runnable threads than cores each crossing waits for
a thread the scheduler set aside, while the waiters burn the cores it
needs. Patched, a waiter spins 1,024 rounds and then sleeps on a futex
until the last thread arrives and wakes it; off Linux it yields
instead. Embedding 60 prompts on four threads on a 4-vCPU VM beside 3
busy processes took 166 s before and 70 s after; beside 6, 286 s and
84 s; idle, 25.7 s and 23.5 s, with the same vectors to the bit.
`datalib/backend/embedder/README.md` § "Threads" has the rest. A bump
re-applies it; if it no longer applies, re-read upstream's barrier first.

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
