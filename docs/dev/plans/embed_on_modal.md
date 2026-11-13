# Embedding a source on Modal's GPUs

**Status: proposal (2026-11-12), nothing built. Facts marked _to check_
have not been measured or read from source yet.**

A library's first embed is slow on the machine it lives on: about 16
hours on an M-series Mac for one large library (710k chunks at ~12 a
second on Metal), and many times that on a CPU-only Linux box or in the
docker image. This plan lets a person send one source's chunks to their
own [Modal](https://modal.com) account to be embedded on a rented GPU,
source by source, off unless they turn it on.

## What a person writes

On a source's `embed` step, and nowhere else:

```toml
[[steps]]
group = "gmail"
function = "embed"
command = "datalib-step"
inputs = ["gmail/keyword_index"]
[steps.params]
embed_on = "modal"        # "local", the default, or "modal"
```

Per source, because it is per source that a person decides what may
leave their machine: a work Slack, maybe; their health data, never.
`embed_on` is a closed set, so it is an enum (`EmbedOn`), and a value
this build does not know is refused by name at load, never read as
"local".

## What leaves the machine

**The text of every chunk the inference cache lacks, for the sources
that say `modal`, and nothing else**: no file, no path, no title but the
one inside each chunk's prompt, no store. What comes back is one vector
per chunk. The vectors go into the inference cache under each prompt's
hash (`docs/dev/plans/search_index_from_ram.md` § "The inference cache"),
so a vector made on Modal is the same entry a local embed would have made,
and the next local `embed` finds it there and skips the chunk.

It goes to the person's own Modal account, signed in the way Modal's own
tools sign in (`~/.modal.toml`, or `MODAL_TOKEN_ID` and
`MODAL_TOKEN_SECRET`). datalib never copies that credential. The text
crosses Modal's TLS connection into a container that lives for the run
and is gone after it; whether Modal keeps a container's stdin or logs is
_to check_, and the step prints nothing of a chunk.

`docs/dev/security_posture.md` gains a section for this: who sees the
text (Modal, as the person's processor), what guards it (the per-source
switch, nothing written there), and what is open (Modal's own retention).

## What runs there: our image, our code

Modal runs a container from an image in a registry, so the remote side is
**`ghcr.io/imbue-ai/datalib`, the docker image we already ship**. It
holds `datalib-step` and the pinned embedding model, which
`datalib-step pull-models` puts in place at build time and which
`DATALIB_NO_FETCH` keeps from being fetched again. A new `datalib-step
embed-prompts` reads `(hash, prompt)` lines on stdin and writes `(hash,
vector)` lines on stdout, through the same `datalib_embedder` the local
step uses, `fit` included. So the prompts, the chunking, the hashing and
the model are one code path wherever they run, and there is no second
embedder to keep in line with the first.

**The image gets a GPU backend.** On Linux our llama.cpp build is
CPU-only today (`third-party/llama_cpp/llama_cpp.BUILD`: Metal on macOS,
nothing else), and a CPU container is no faster than the machine the
library is on. So the first thing built is **ggml's CUDA backend under
Bazel**, for Linux x86_64:

- NVIDIA's toolkit as a pinned, sha256-checked download, like every other
  toolchain here, and its rules (likely `rules_cuda`), so the build stays
  hermetic and CI can build it with no GPU.
- `ggml-cuda`'s sources in `llama_cpp.BUILD` behind a `--config=cuda`
  flag, the kernels compiled for the GPU generations Modal rents
  (which ones is _to check_); the embedder picks the GPU as it picks
  Metal on a Mac (`Device::Best`).
- A GPU variant of the image, `ghcr.io/imbue-ai/datalib:<tag>-cuda`, with
  the CUDA runtime libraries it needs and nothing else changed. The plain
  image stays as it is: no one running the image on a CPU pays for CUDA.
- The parity test gains a CUDA case, its vectors against the CPU's by
  cosine as Metal's are today, run on a Modal GPU, since CI has none.

NVIDIA's runtime libraries are not open source; what their license lets
us ship inside the image, and what the release's notices must say, is
_to check_ before the image is published (`AGENTS.md` § "License: MIT,
and what may come in").

## How the step drives Modal

`datalib-step` is Rust and Modal's main client is Python, which we do not
ship. Options, in order of preference:

- **Modal's JavaScript SDK on the Node we bundle** (`docs/dev/runtime_fetch.md`).
  Whether that SDK can start a container from a registry image with a GPU
  and stream its stdin and stdout, as Modal's Python sandboxes can, is
  _to check_. If it can, a small script beside the step does the Modal
  half, as `qmd_sdk.mjs` does qmd's today, and nothing is installed.
- **Modal's `modal` command**, if the person has it. Simple, but then a
  feature depends on a Python tool we do not pin.

Either way the step owns the work: it asks the cache what is missing,
sends those prompts in slices, writes what comes back as it arrives, and
counts what failed. An interrupted run loses nothing already written,
and the next run sends only what is still missing.

## What a person notices

- With `embed_on = "modal"` on a source, its first embed takes minutes
  rather than hours, and shows the same progress bar.
- Without a Modal sign-in, the step fails and says how to sign in; it
  does not quietly embed locally, which would cost the hours the person
  chose not to spend.
- A search reads these vectors as any other; the vectors from Modal's
  GPU and from a Mac differ in the last digits, as Metal's and the CPU's
  do (the parity test's bound).

## Order of work

1. The CUDA build: `ggml-cuda` under Bazel behind `--config=cuda`, the
   pinned toolkit, the embedder choosing the GPU; built in CI, run by
   hand on a Modal GPU through the parity test.
2. `datalib-step embed-prompts`: stdin to stdout over `datalib_embedder`,
   tested here on the CPU against the step's own vectors.
3. The `-cuda` image, its license notices, and the release job that
   builds and pushes it.
4. The Modal half behind `embed_on = "modal"`, measured on one small
   source of a real library, then a large one.
5. The security posture section, and `docs/dev/config_model.md` and
   `configs/dag_example.toml` for the switch.
