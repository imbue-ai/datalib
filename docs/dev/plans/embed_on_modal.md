# Embedding a source on Modal's GPUs

**Status: proposal (2026-11-12). Steps 1 and 2 are done: the CUDA module
under Bazel (#1750), the embedder loading it (#1752), the module run on
Modal's T4, A100 and H100 in the published image with several prompts
per call (#1801, § "Measured on Modal"), and `datalib-step
embed-prompts`. Step 3 shipped in v0.55.0 (2026-11-18): the module
as a release asset with NVIDIA's licenses beside it, the `-cuda` image
whose CUDA layers keep their digests from one release to the next
(docs/dev/docker.md § "The CUDA variant"), and a tarball install
fetching the module on first use (docs/dev/runtime_fetch.md § "The CUDA
module"). Step 4, `embed_on_modal_from_n_chunks` and the driver in
`datalib/backend/datalib_step/src/modal/`, has run against Modal on the
TNG fixture and on a real library, where a GPU Sandbox's stdout lost
answers; slices now move as files, and 5,000 made-up prompts came back
whole (§ "Measured on Modal"). Step 5 is done:
docs/dev/security_posture.md § "What leaves the machine for Modal".
Facts marked _to check_ have not been measured or read from source
yet.**

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
inputs = ["unified_grid_and_search_index/grid_index"]
[steps.params]
embed_on_modal_from_n_chunks = 5000
```

**Absent, nothing of the source leaves the machine.** Present, a run that
owes at least that many chunks (prompts the inference cache has no
vector for) sends all of them to Modal; a run that owes fewer embeds
them here. So a source's first embed, the backfill, goes to Modal, and
the few new chunks of a daily sync stay local, where a Modal container's
start would cost more than it saves. 0 sends every run. A run that falls
under the threshold after an interrupted one finishes locally, since it
is counted on what that run owes.

Per source, because it is per source that a person decides what may
leave their machine: a work Slack, maybe; their health data, never. It
is the first parameter of an `embed` step, so `check-params` learns to
plan it and refuse a value that is not a whole number.

The 5,000 in the example is a starting point, not a measurement: about
seven minutes of a Mac's Metal at 12 chunks a second, against a Modal
container's start (the image, then the model onto the GPU). One start has
been measured: about 107 s, the first pull of the image on that account
(§ "Measured on Modal"). A start with the image already on Modal is
_to check_.

## What leaves the machine

**The text of every chunk the inference cache lacks, for the sources
that set `embed_on_modal_from_n_chunks` and on a run that owes at least
that many, and nothing else**: no file, no path, no title but the
one inside each chunk's prompt, no store. What comes back is one vector
per chunk. The vectors go into the inference cache under each prompt's
hash (`docs/dev/plans/completed/search_index_from_ram.md` § "The inference cache"),
so a vector made on Modal is the same entry a local embed would have made,
and the next local `embed` finds it there and skips the chunk.

It goes to the person's own Modal account, signed in the way Modal's own
tools sign in (`~/.modal.toml`, or `MODAL_TOKEN_ID` and
`MODAL_TOKEN_SECRET`). Datalib never copies that credential. The text
crosses Modal's TLS connection into a container that lives for the run
and is gone after it, the slice a file in its filesystem; whether Modal
keeps a container's files or logs is _to check_, and the step prints
nothing of a chunk.

`docs/dev/security_posture.md` § "What leaves the machine for Modal" is
the section for this: who sees the text (Modal, as the person's
processor), what guards it (the per-source parameter, absent by default,
the driver's named environment, nothing written there), and what is open
(Modal's own retention, a killed driver's Sandboxes, the image pulled by
tag).

## What runs there: our image, our code

Modal runs a container from an image in a registry, so the remote side is
**the docker image we already ship, in its CUDA variant,
`ghcr.io/imbue-ai/datalib:<version>-cuda`**: the image a person with an
NVIDIA GPU would run themselves. It is the plain image (`docs/dev/docker.md`)
with two more layers, for `linux/amd64` only: `libggml-cuda.so` in
`/opt/datalib/lib/`, and cuBLAS 12. The plain image, the one everyone
else pulls, does not grow. Both hold `datalib-step` and the pinned
embedding model, which `datalib-step pull-models` puts in place at build
time and which `DATALIB_NO_FETCH` keeps from being fetched again. A
variant is needed at all because NVIDIA's container runtime passes a
container the driver (`libcuda.so.1`) and nothing else: without cuBLAS
in the image, `--gpus all` still embeds on the CPU. `datalib-step
embed-prompts` reads one `{"hash", "prompt"}` JSON object a line on
stdin and writes one `{"hash", "vector", "cut"}` object a line on
stdout, in order and 64 at a time, through the same `datalib_embedder`
the local step uses, `fit` and the device's batching included. A vector
is about 8 KB of JSON (768 floats, each exact through the round trip),
so 710k of them are some 5.6 GB back from the GPU. So the prompts, the chunking, the hashing and
the model are one code path wherever they run, and there is no second
embedder to keep in line with the first.

**The Linux build gets a GPU backend, loaded only where there is a GPU.**
On Linux our llama.cpp build is CPU-only today
(`third-party/llama_cpp/llama_cpp.BUILD`: Metal on macOS, nothing else),
and a CPU container is no faster than the machine the library is on. So
the first thing built is **ggml's CUDA backend under Bazel**, for Linux
x86_64, as a module the one gnu build loads when it can:

- **One binary, CPU and CUDA.** ggml can load a backend at run time
  (`ggml_backend_load`, a `dlopen`), beside the CPU backend linked in.
  Upstream links the binary and the module against a shared
  `libggml-base.so`; ours keeps ggml linked into the binary, which
  exports its symbols on gnu Linux, and the module takes ggml from the
  program that loads it, so the binaries stay self-contained (#1752).
  The CUDA backend is `libggml-cuda.so`, looked for in `lib/` beside the
  binary. The embedder loads it when it is there and the module loads, and otherwise runs on the CPU as today,
  saying which it chose and, when the module would not load, why. So
  the same `datalib-step` runs on a laptop, a CPU container and a GPU
  one.
- **cuBLAS comes from the host.** Whoever runs CUDA has NVIDIA's
  libraries installed, so the module links `libcublas.so.12` and
  `libcublasLt.so.12` from the host (from the toolkit, or NVIDIA's
  `nvidia-cublas-cu12` package), as node-llama-cpp's CUDA build does;
  besides those it needs only the driver's `libcuda.so.1`, and the
  small CUDA runtime is linked in. Linked in statically, cuBLAS made the
  module 939 MB, and `nvprune` only took it to 737 MB (#1750). A
  tarball install on a GPU machine has the toolkit or the wheel; the
  `-cuda` image carries cuBLAS itself. In the smoke test (§ "Measured on
  Modal") it came from NVIDIA's `nvidia-cublas-cu12` wheel (12.9, 581 MB
  with nvrtc's 90 MB), its `nvidia/cublas/lib` on `LD_LIBRARY_PATH`;
  whether the `-cuda` image takes it from that wheel or from NVIDIA's
  redistributable tarballs, as the build takes the toolkit, is _to
  check_. Modal's GPU hosts give the driver's `libcuda.so.1`.
- **The module ships apart from the tarball.** Without cuBLAS it is
  about llama.cpp's GPU code alone (node-llama-cpp's is 156 MB for more
  GPU models; ours is 122 MB for five), still more than everyone else
  should carry. It is one more release asset: a tarball install fetches
  it on first use where there is a GPU (`libcuda.so.1` on the loader
  path; `docs/dev/runtime_fetch.md` § "The CUDA module"), and the
  `-cuda` image has it built in, so a container never fetches it.
- **musl stays CPU-only.** A fully static musl binary cannot `dlopen` a
  glibc library, and NVIDIA's libraries are glibc builds.
- NVIDIA's toolkit as a pinned, sha256-checked download (NVIDIA publishes
  each component as a redistributable tarball with a JSON manifest of
  hashes), and its rules (likely `rules_cuda`), so the build stays
  hermetic and CI can build it with no GPU. It cannot be built on a Mac:
  `nvcc` does not run there, so the loop is CI or a Linux box.
- The kernels compiled for the GPU generations Modal rents: 7.5 (T4),
  8.0 (A100), 8.6 (A10G), 8.9 (L4, L40S) and 9.0 (H100, H200), with
  9.0's PTX for newer ones, which the driver compiles on first load.
- `gpu_check` (`datalib/backend/embedder/tools/gpu_check.rs`), which
  `cuda.yml` builds and uploads beside the module, holds a GPU's vectors
  to the CPU's by cosine, run by hand on a Modal GPU, since CI has none.

NVIDIA's libraries are not open source. The module carries the CUDA
runtime and the `-cuda` image cuBLAS, both of which the CUDA Toolkit
EULA's Attachment A lists as redistributable. Each archive carries
`third-party/llama_cpp/CUDA-NOTICE.md`, which says their use is governed
by that EULA and not by Datalib's MIT license, and NVIDIA's own
`LICENSE` for the component, verbatim from the archive Bazel fetched it
in. Whether a release publishes them is the user's decision
(`AGENTS.md` § "License: MIT, and what may come in").

## Measured on Modal

`gpu_check` from `cuda.yml`, with the module in `lib/` beside it, in
`ghcr.io/imbue-ai/datalib:0.54.1` plus the cuBLAS layer above, over the
TNG fixture's 484 prompts (117,839 tokens) and the CPU over the first
352. The module loaded on every GPU (`CUDA0`; drivers 610.57 and
620.70), with cuBLAS from the wheel and nothing else from the host.

| device | prompts/s | tokens/s | $ per M tokens |
|---|---|---|---|
| Modal T4 | 41–43 | 10,000–10,500 | 0.016 |
| Modal A100 40 GB | 56.8 | 13,800 | 0.042 |
| Modal H100 | 87.8 | 21,400 | 0.051 |
| M-series Mac, Metal | 20.6–27.6 | 5,000–6,700 | — |
| Modal's x86 CPU, in the GPU container | 2.0–3.1 | 470–700 | — |

One prompt per call, the first call's start included. The price is
Modal's list price per GPU-second over tokens per second, CPU and memory
left out. An L4 was asked for and not given within 40
minutes, so it is not here.

**Agreement.** CUDA against Metal: worst cosine 0.99913, mean 0.99973,
as close as any two devices here are to each other (Metal against the
Mac's CPU: 0.99936). CUDA against either CPU: 0.9983 on one 26-token
prompt, 0.9993 or better on every other, the same on all three GPUs.
`gpu_check` holds CUDA to the CPU at 0.998.

**Several prompts per call.** embeddinggemma keeps no cache in
llama.cpp, so one call attends over every token in it, each prompt
masked from the others: a call's time and scratch memory grow with the
square of its tokens. The embedder packs prompts in order into calls of
at most a count and a token budget (`Batching` in
`datalib/backend/embedder/src/lib.rs`), and a prompt's vector does not
depend on its neighbours (`tests/parity.rs`). Over the same 484 prompts,
the first call left out of the rate:

| device | 1 per call | 8 per call, 2,048 tokens | 16 per call, 4,096 | 128 per call, 32,768 |
|---|---|---|---|---|
| T4 | 44.3/s | 51.0/s | 29.5/s | 6.9/s |
| A100 40 GB | 81.3/s | 146.5/s | 82.0/s | 26.4/s |
| H100 | 97.1/s | 145.7/s | 82.2/s | 25.1/s |
| Metal (M-series) | 25.6/s | 27.1/s | 23.2/s (8 per call) | 26.2/s (32 per call, 8,192) |

So the default is 8 prompts in one context's tokens (2,048) on CUDA,
and one per call on the CPU and Metal. On Metal a call of 2,048 tokens
peaks at 2.3 GB however many prompts share it, 4,096 at 4.5 GB and 8,192
at 8 to 9 GB, none faster: what made qmd's batching cost memory for
nothing. The A100 and H100 tie, so at this size something other than the
GPU's arithmetic bounds them (_to check_: the host side of each call).
CUDA against the CPU did not move with the batching (worst cosine
0.9992 to 0.9995 over 64 prompts).

**Cheapest per token is still the T4, but barely.** At 8 per call: T4
$0.013 per million tokens, A100 $0.016, H100 $0.031. A library of 710k
chunks at ~560 tokens each (12 chunks a second at Metal's rate above;
_to check_ on the library itself) is about 400M tokens: some $5 on T4s
(9 GPU-hours) or $7 on A100s (3 GPU-hours). These prompts average 243
tokens, so 8 share a call; a library's longer chunks share less, and
gain less.

**Spread across containers.** Each prompt is embedded alone and stored
under its own hash, so one source's missing prompts split into slices,
one container per slice, with no step between them but the writes the
local step makes as slices come back. The GPU-seconds, and so the cost,
are the same as one container's; the wait divides by the number of
containers, plus each one's start (the image and the model loaded on
the GPU, _to check_ at scale). Twenty T4s would take the library above
in about half an hour. What caps it is the account's concurrency limit
and what Modal has free of a GPU type (_to check_).

**Through the step, once.** On 2026-11-18, `datalib-step` built from
`main` (version 0.55.0) ran `claude-api/embed` over a TNG fixture root
with its inference cache deleted and `embed_on_modal_from_n_chunks = 0`.
It used v0.55.0's Node runtime (Modal's SDK 0.11.1), `~/.modal.toml` as
the sign-in, and the default image and GPU. All 74 owed chunks (16,086
bytes of prompt text) went in one slice to one T4 Sandbox from
`ghcr.io/imbue-ai/datalib:0.55.0-cuda`, and the step said: "74 prompts on
T4 in 108.9 s", then "1 T4 Sandbox(es), 1 min 49 s of Sandbox time, about
$0.02 at list price". At the T4's rate above, the embedding itself is
about 2 s of that, so nearly all of it is the Sandbox's start: pulling
the image, which this account had not pulled before, and loading the
model. Every vector came back, and against the fixture's own vectors for
the same chunks the worst cosine was 0.99966 and the median 0.99977.
Still to measure: a start once the image is on Modal, many slices at
once, and the estimate against the bill.

**A GPU Sandbox's stdout loses a large answer.** On a real library the
same day, every slice came back short: 950 and 885 vectors for one slice
of 4,096 prompts, 1,533 and 1,537 for one of 3,910, each from an
`embed-prompts` that exited 0. Probes on made-up data in the plain image
found where. 8 MB sent through a Sandbox's stdin arrived whole, on a CPU
and on a T4, read at once or 64 lines at a time. 32 MB written to stdout
over two minutes after 60 s of silence arrived whole from a CPU Sandbox,
and from a T4 Sandbox as 702 of its 4,096 lines, with three gaps and no
error. The same 32 MB written to a file in the T4 Sandbox and read with
the SDK's `filesystem` calls arrived whole, in 34 s. So a slice moves as
files (§ "How the step drives Modal"), and then 5,000 made-up prompts of
about 2 KB came back as 5,000 vectors from two T4 Sandboxes: 4,096 in
132.8 s and 904 in 96.2 s. Against a Mac's Metal on 50 of them, the
worst cosine was 0.99833.

**"The Sandbox is unavailable" was a running Sandbox.** On 2026-11-25 one
embed of 10,747 chunks started 65 T4 Sandboxes and 26 of them failed with
"The Sandbox is unavailable. This Sandbox may have already shut down.",
some seconds after their start, and 768 chunks were left over. Modal's own
list of the app's Sandboxes (`SandboxList` with the finished ones, which
costs nothing to read) says what happened to each: it was placed, it was
running about a second later, and it ended 10.3 to 11.3 s after that,
"cancelled by user", which was our driver. Eight failures on 2026-11-19
look the same. Ten seconds is the SDK's retry budget for one call to a Sandbox: ten
retries from 10 ms, doubling, 10.23 s in all. Its file calls then throw
that message for every code they retried (`UNAVAILABLE`, `CANCELLED`,
`UNKNOWN`, `DEADLINE_EXCEEDED`) and drop the code, so a Sandbox that does
not answer yet reads as one that is gone, and the driver ended it.

- **Modal did not refuse so many at once.** Of six runs of 100 Sandboxes
  or more, five had no such failure and one had one. A Sandbox that waits
  for a GPU waits inside `create`, which returns once it is placed (up to
  56 s later in this run), so no call is made to one still queued.
- **Why they were silent past 10 s is not known**, since the code was
  dropped: the container may still have been starting, or the host it was
  on could not be reached. It was 26 of 65 here and 8 of 753 over
  2026-11-18 to 20, on the release before. The driver's first call is now
  an `exec`, whose error carries the code, and it logs it, so the next run
  that meets it says.
- **What Modal bills starts when a Sandbox is placed.** The driver counted
  from before that: 29 min for this run, of which 18 min were placed. It
  counts from there now.
- **On 2026-11-24 the laptop's lid closed** as 42 Sandboxes started. Awake
  for a moment 6 minutes later and with no network, the driver called all
  42 unavailable, could not end them either, and spent every second attempt
  on a `create` that found no network. All 42 ran their full 15 minutes.

## How the step drives Modal

`datalib-step` is Rust, Modal has no Rust SDK, and its Python SDK, the
only one that can define or deploy a Function, is a tool we do not
ship or pin. Its JavaScript SDK (`modal` on npm, beta; the Go SDK has
the same scope) runs on the Node we bundle (`docs/dev/runtime_fetch.md`)
and can do two things that matter here: call a Function someone already
deployed, and start a **Sandbox**, a container from a registry image
with a GPU, and write to its stdin and read its stdout. It cannot
deploy a Function, and cannot `map` one.

So the step's Modal half is a small script beside the step on that Node,
and it uses Sandboxes. The script is
`datalib/backend/datalib_step/src/modal/embed_on_modal.mjs`, compiled
into `datalib-step` and run with `node --eval`; it imports npm's `modal`
(pinned in `third-party/modal/runtime/`, staged into the runtime as
`modal/<v>/`) by its path, and the step's side is `src/modal.rs` and
`embed_on_modal` in `src/search_index.rs`:

- **The step asks the cache what is missing**, as it does locally, and
  writes those prompts to the driver's stdin as it reads the documents;
  the driver cuts them into slices. A slice is what is owed spread over
  every Sandbox allowed, but at least 256 prompts, so a Sandbox's start
  is not most of what it is paid for, and at most 4,096, a minute or two
  of a T4 (`slice_for` in `src/modal.rs`).
- **One Sandbox per slice**, from the `-cuda` image with a GPU (a T4;
  at most the config's `[modal] max_sandboxes` at once, 100 when it
  names none), under the Modal App `datalib-embed`. The Sandbox idles
  (`sleep`); the driver writes the slice to `/tmp/datalib-embed/in.jsonl`
  with `filesystem.writeText`, runs `/opt/datalib/datalib-step
  embed-prompts --models-dir /root/.cache/qmd/models` (the model the
  image bakes in) with `exec`, its stdin and stdout on files, and reads
  its output file once it has exited. Nothing crosses the Sandbox's own
  stdin or stdout, which on a GPU lost answers (§ "Measured on Modal").
  `DATALIB_MODAL_IMAGE`, `DATALIB_MODAL_GPU` and
  `DATALIB_MODAL_SANDBOXES` change the image, the GPU and how many run
  at once, for measuring; the last wins over the config's. Splitting one embed across many cheap GPUs costs the same
  GPU-seconds as one and divides the wait (§ "Measured on Modal").
- **Each Sandbox's vectors go into the inference cache as it returns**,
  checked line by line against what it was sent: a line that answers a
  prompt of its slice is kept, each prompt once. The prompts still
  missing go to a second Sandbox, and what is missing after that is
  reported, and the step counts it.
- **A Sandbox that does not answer is asked again, not replaced.** Each
  call to one is made again, after pauses of 1 s doubling to 30 s, while
  Modal says it is running (`poll`), for about two minutes; a Sandbox
  Modal says has ended is replaced at once. While Modal itself cannot be
  asked, which is a laptop with no network, the calls go on until the
  Sandbox's 15 minutes are over, and a `create` that finds no network is
  tried for ten minutes of waiting before it counts as a failure. The
  waits are added up by their length, not the clock, so a laptop that
  sleeps for an hour has not used them up. A Sandbox is ended the same
  way, so one is not left running because the network was gone. A sleep
  longer than a Sandbox's life still loses its slice; the second Sandbox
  does it again (#1815).
- **Each Sandbox asks only for its GPU**, so Modal reserves its minimum
  CPU and memory and bills whatever more the run uses. The driver
  reports each Sandbox as it ends: how long it lived, from when Modal
  placed it (the wait for a GPU is not billed) to its end, and whether
  its slice came back. A failed attempt ran, so it
  is counted too.
- **What the run spent is reported as it goes**, as running totals the
  sync dashboard draws and `/metrics` exports: `modal_chunks_total`,
  and by GPU `modal_sandboxes_total`, `modal_sandboxes_failed_total`,
  `modal_sandbox_seconds_total` and `modal_cost_estimate_micro_usd_total`.
  The estimate is each Sandbox's life priced at Modal's list price for
  its GPU plus an assumed core and 2 GiB of use, at the Sandbox's rate
  (`modal::estimated_micro_usd`). That use is set high on purpose, since
  nothing has measured it and a person choosing a threshold is better
  served by a figure that errs high: a T4 Sandbox comes to $0.78 an hour,
  of which the GPU is $0.59. The table in `src/modal.rs` was read from
  modal.com/pricing on 2026-11-18 and has to follow the page. A Sandbox
  the driver could not end (a SIGKILL) goes uncounted. _To check_
  against the bill on the first live run, which also measures what a
  Sandbox uses, so the assumption can be set from that.
- **The Manage screen's Modal spend column** sums the estimate over the
  runs the run store keeps, per `embed` step and for its group, with
  the total climbing run by run behind it, and the latest run's
  Sandboxes and Sandbox time in its hover (`http/src/manage/spend.rs`).
  It hides while no row has spent anything.
- **The run ends with one line** on its Manage row's Status (a hover)
  and in its log: "embedded 8192 chunks on Modal: 3 T4 Sandbox(es), 6
  min 0 s of Sandbox time, about $0.08 at list price". A failed run puts
  the same words in its error instead, since the row shows the error.
- **Nothing about the remote work is stored** but how much of it there
  was (the metrics above): no Sandbox to go back to. An interrupted run loses
  the slices in flight and nothing already written: a stop sends the
  driver SIGTERM, and it ends every Sandbox it asked for before it
  exits. That includes those still waiting for a GPU, which the SDK's
  `create` names only once Modal has placed them: each Sandbox is made
  with a tag naming this run of the driver (`datalib_embed_run`), and the
  stop lists the app's running Sandboxes with that tag and ends them.
  Left alone, one would start after the run had gone and idle, billed,
  to its limit. Measured on 2026-11-26 with three T4 Sandboxes stopped
  before Modal placed any: the list found all three and each was
  cancelled before it started. One asked for in the instant of the
  stop, before Modal has listed it, can still be missed. The next run owes what is still
  missing and sends only that.
- **The driver is handed only what it needs** of the step's environment:
  what every child gets, and Modal's own variables (`MODAL_TOKEN_ID`,
  `MODAL_TOKEN_SECRET`, `MODAL_PROFILE`, `MODAL_CONFIG_PATH`,
  `MODAL_ENVIRONMENT`, `MODAL_SERVER_URL`; `datalib_env::Child::Modal`).
  A sync's runner hands a step none of those, so under a sync the
  sign-in is `~/.modal.toml`, unless `DATALIB_CHILD_ENV` names them.
- **A Sandbox lives at most 15 minutes.** A driver killed outright
  (SIGKILL, a crash) cannot end its Sandboxes; they stop at that limit,
  which is also how long the idle `sleep` runs.

A Sandbox's GPU-seconds cost what a Function's do; its CPU and memory
cost three times a Function's, a small share of the bill. Its output is
gone if the step stops watching, which is why nothing is kept for later.
A Sandbox pulls from `ghcr.io` (Modal's examples use Docker Hub): the
run above did. _To check_: the largest file `filesystem.writeText` and
`readText` carry (the SDK names a read size limit; 32 MB passed), and
whether the SDK's next release fixes a GPU Sandbox's stdout.

**TODO (#1815): durable batch jobs, once we can reach them.** On a
deployed Function, Modal's `spawn()` and `spawn_map()` keep running when
the caller leaves, keep each `spawn()` result for 7 days, queue up to a
million inputs, and scale out on their own. With those, a laptop that
sleeps mid-backfill would lose nothing. They need the Function deployed
from Python, or `map` from the JavaScript SDK, which "is not currently
supported"; the issue says what to check for in a new SDK release.

## What a person notices

- With `embed_on_modal_from_n_chunks` on a source, its first embed takes
  minutes rather than hours, and shows the same progress bar; its daily
  embeds run here as before.
- While it runs, the row's Status says "search index: embedding on
  Modal". When it ends, it says how many chunks went, on how many
  Sandboxes of which GPU, for how long and about what that cost at list
  price; the source's sync dashboard draws the Sandboxes, their seconds
  and the estimated dollars over the run.
- A run over the threshold without a Modal sign-in fails and says how to
  sign in or raise the threshold; it does not quietly embed locally,
  which would cost the hours the person chose not to spend.
- A search reads these vectors as any other; the vectors from Modal's
  GPU and from a Mac differ in the last digits, as Metal's and the CPU's
  do (`datalib/backend/embedder/README.md`).

## Order of work

1. ~~The CUDA build: `libggml-cuda.so` under Bazel
   with the pinned toolkit, the embedder loading it when it can; built
   in CI, run by hand on a Modal GPU.~~ Done; § "Measured on Modal".
2. ~~`datalib-step embed-prompts`: stdin to stdout over `datalib_embedder`,
   tested here on the CPU against the step's own vectors.~~ Done:
   `datalib_step/src/embed_prompts.rs`, and its end-to-end test in
   `tests/step_tests/embed_prompts.rs` against the TNG fixture's cache.
3. ~~The module as a release asset, its license notices, the fetch for a
   tarball install, and the `-cuda` image with the module and cuBLAS.~~
   Done: v0.55.0 carries `ggml-cuda-x86_64-unknown-linux-gnu.tar.gz`
   and `ghcr.io/imbue-ai/datalib:0.55.0-cuda`, built by the release's
   `cuda`, `docker-image-cuda` and `docker-publish-cuda` jobs. Its first
   tag failed in `cuda`, and #1853 fixed that before the tag was cut
   again. What the `cuda` job costs a release is #1861.
4. The Modal half behind `embed_on_modal_from_n_chunks`: Sandboxes from
   the `-cuda` image driven by the JavaScript SDK. Tested against a
   stand-in for the SDK whose Sandboxes run locally, and run against
   Modal on the TNG fixture, on a real library, and on 5,000 made-up
   prompts (§ "Measured on Modal"); to be measured on a real library's
   sources again with slices moving as files, which is where the
   _to check_ items above get answered.
5. ~~The security posture section, and `docs/dev/config_model.md` and
   `configs/dag_example.toml` for the parameter.~~ Done.
