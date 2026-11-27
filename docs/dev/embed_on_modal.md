# Embedding a source on Modal

A source's `embed` step turns each chunk of its documents into a vector
for search by meaning. The first embed of a large library is slow on the
machine it lives on: about 16 hours on an M-series Mac for 710k chunks at
some 12 a second on Metal, and many times that on a Linux box with no
GPU or in the docker image. So an `embed` step can send what it owes to
the person's own [Modal](https://modal.com) account, to be embedded on
rented GPUs. It is per source, and off unless the person turns it on.

Words used here:

- **Chunk** and **prompt**: a piece of a document, and the text the
  model embeds for it (the document's title and the chunk's text). Each
  prompt is known by its hash.
- **Owed**: the prompts the inference cache has no vector for.
- **Sandbox**: a container Modal runs from a registry image, with a GPU,
  that our code talks to while it lives.
- **Slice**: the prompts one Sandbox is handed.

## What a person writes

On a source's `embed` step, and nowhere else:

```toml
[[steps]]
id = "slack/embed"
command = "datalib-step run embed slack"
inputs = ["unified_grid_and_search_index/unified_grid_and_search_index"]
[steps.params]
embed_on_modal_from_n_chunks = 5000
```

**Absent, nothing of the source leaves the machine.** Present, a run
that owes at least that many prompts sends all of them to Modal, and a
run that owes fewer embeds them here (`modal::route`). So a source's
first embed goes to Modal and the few new chunks of a daily sync stay
local, where a Sandbox's start would cost more than it saves. `0` sends
every run. A run counts only what it owes, so a run after an
interrupted one may fall under the threshold and finish here.

The parameter is a whole number and the step's only one;
`datalib-pipeline check` refuses anything else (`datalib_step/src/check.rs`).
What every such step shares is in the config's `[env]`:
`DATALIB_STEP_MODAL_MAX_SANDBOXES`, how many Sandboxes one step runs at
once, 100 when it is not set
([`config_model.md`](config_model.md), [`step_protocol.md`](step_protocol.md)).

The 5,000 in the example is about seven minutes of a Mac's Metal,
against a Sandbox start that took about 107 s the first time an account
pulled the image.

## What leaves the machine

The text of every owed prompt and its hash, for a source that sets the
parameter and on a run that owes at least that many, and nothing else.
What comes back is one vector per prompt, which goes into the inference
cache under the prompt's hash, the entry a local embed would have made,
so the next local `embed` finds it and skips the chunk. It goes to the
person's own Modal account, signed in the way Modal's own tools sign in.
Who sees the text, what guards it and what is not guarded:
[`security_posture.md`](security_posture.md) § "What leaves the machine
for Modal".

## What runs there

**The `-cuda` docker image of this release,
`ghcr.io/imbue-ai/datalib:<version>-cuda`**, the image a person with an
NVIDIA GPU would run themselves ([`docker.md`](docker.md) § "The CUDA
variant"). It holds `datalib-step`, the pinned embedding model, the CUDA
module and cuBLAS. Each Sandbox runs `datalib-step embed-prompts`
(`datalib_step/src/embed_prompts.rs`): one `{"hash", "prompt"}` JSON
object a line on stdin, one `{"hash", "vector", "cut"}` a line on
stdout, in order and 64 at a time, through the same `datalib_embedder`
the local step uses. So the prompts, the hashing and the model are one
code path wherever they run, and there is no second embedder to keep in
line with the first.

**The CUDA module** is llama.cpp's CUDA backend, `libggml-cuda.so`,
built by Bazel (`third-party/llama_cpp/llama_cpp.BUILD`, with NVIDIA's
toolkit from its redistributable manifest through `rules_cuda` in
`MODULE.bazel`):

- **One binary runs on the CPU and on CUDA.** A gnu Linux build of
  `datalib-step` loads the module at start when it finds one and it
  loads, and otherwise runs on the CPU, saying which and why. The module
  takes ggml from the program that loads it, so the binaries stay
  self-contained. Where it is looked for:
  [`runtime_fetch.md`](runtime_fetch.md) § "The CUDA module" and
  [`embedder/README.md`](../../datalib/backend/embedder/README.md).
- **cuBLAS comes from the host**, as do the driver's `libcuda.so.1`; the
  small CUDA runtime is linked in. Linked in statically, cuBLAS made the
  module 939 MB. The `-cuda` image carries cuBLAS itself.
- **Its kernels are for the GPUs Modal rents**: compute capability 7.5
  (T4), 8.0, 8.6 and 8.9 (A100, A10G, L4, L40S) and 9.0 (H100, H200),
  with 9.0's PTX for newer ones, which the driver compiles on first
  load.
- **musl stays CPU-only**: a static musl binary cannot load a glibc
  library, and NVIDIA's are glibc builds.
- **It cannot be built on a Mac**, where `nvcc` does not run. CI builds
  it (`.github/workflows/cuda.yml`, and the release's `cuda` job:
  [`release_steps.md`](release_steps.md)); CI has no GPU, so it is run
  on a GPU by hand with `gpu_check`
  (`datalib/backend/embedder/tools/gpu_check.rs`), which holds a GPU's
  vectors to the CPU's.

NVIDIA's libraries are not open source. The module carries the CUDA
runtime and the `-cuda` image cuBLAS, both redistributable under the
CUDA Toolkit EULA; each archive carries
[`CUDA-NOTICE.md`](../../third-party/llama_cpp/CUDA-NOTICE.md) and
NVIDIA's own `LICENSE` for the component.

## How the step drives Modal

`datalib-step` is Rust and Modal has no Rust SDK. Its Python SDK, the
only one that can deploy a Function, is a tool we do not ship. Its
JavaScript SDK (npm's `modal`) runs on the Node we bundle and can start a
Sandbox from a registry image, with a GPU, and talk to it; it cannot
deploy a Function or `map` one. So the step's Modal half is a small
driver on that Node, `datalib_step/src/modal/embed_on_modal.mjs`,
compiled into `datalib-step` and run with `node --eval`. It imports the
SDK pinned in `third-party/modal/runtime/` and staged into the runtime
as `modal/<version>/` ([`runtime_fetch.md`](runtime_fetch.md)). The
step's side is `datalib_step/src/modal.rs` and `embed_on_modal` in
`datalib_step/src/search_index.rs`.

- **The step asks the cache what is owed**, as it does locally, and
  writes those prompts to the driver's stdin as it reads the documents.
  The driver cuts them into slices: what is owed spread over every
  Sandbox allowed, but at least 256 prompts, so a Sandbox's start is not
  most of what it is paid for, and at most 4,096, a minute or two of a
  T4 (`modal::slice_for`).
- **One Sandbox per slice**, from the `-cuda` image with a T4, under the
  Modal App `datalib-embed`, as many at once as
  `DATALIB_STEP_MODAL_MAX_SANDBOXES` allows. Modal's own limit for the
  account may hold a run to fewer; the rest wait their turn there.
- **A slice moves as files, never through the Sandbox's stdin and
  stdout.** The Sandbox idles (`sleep`). The driver writes the slice to
  `/tmp/datalib-embed/in.jsonl` with the SDK's `filesystem` calls, runs
  `/opt/datalib/datalib-step embed-prompts --models-dir
  /root/.cache/qmd/models` (the model the image bakes in) with `exec`,
  its stdin and stdout on files, and reads the output file once it has
  exited. A GPU Sandbox's stdout loses most of a large answer and still
  ends cleanly: 32 MB written there arrived as 702 of its 4,096 lines
  with no error, where the same bytes read as a file arrived whole.
- **What comes back is checked line by line** against what was sent: a
  line that answers a prompt of the slice is kept, each prompt once.
  The prompts still missing go to a second Sandbox; what is missing after
  that is reported as failed, and the run fails saying how many, with
  what came back already cached.
- **A Sandbox that does not answer is asked again, not replaced.** The
  SDK retries one call for about ten seconds (ten tries from 10 ms,
  doubling), then throws "The Sandbox is unavailable" for a Sandbox that
  is gone and one that is only not answering yet, with the error's code
  dropped. So each call is made again, after pauses of 1 s doubling to
  30 s, while Modal says the Sandbox is running (`poll`), for about two
  minutes; one Modal says has ended is replaced at once. The driver's
  first call to a Sandbox is an `exec`, whose error keeps its code.
- **A laptop with no network does not use up the waits.** While Modal
  itself cannot be asked, the calls go on until the Sandbox's life is
  over, and a `create` that finds no network is tried for ten minutes of
  waiting before it counts as a failure. The waits are added up by their
  length, not by the clock, so an hour asleep spends none of them.
  Ending a Sandbox waits out the network the same way, since one not
  ended is billed to its limit.
- **A Sandbox lives at most 15 minutes**, which is also how long its
  idle `sleep` runs. A sleep longer than that loses its slice, and the
  second Sandbox does it again.
- **Each Sandbox asks only for its GPU**, so Modal reserves its minimum
  CPU and memory and bills what more the run uses.
- **A stop ends every Sandbox the run asked for.** The step sends the
  driver SIGTERM; the driver ends the Sandboxes it holds and, since the
  SDK names a Sandbox only once Modal has placed it, lists the app's
  Sandboxes carrying this run's tag (`datalib_embed_run`) and ends those
  still waiting for a GPU too. One asked for in the instant of the stop,
  before Modal lists it, can still be missed. A driver killed outright
  (SIGKILL, a crash) ends nothing; its Sandboxes stop at their limit and
  go uncounted.
- **Nothing about the remote work is stored** but how much of it there
  was: no Sandbox to go back to. An interrupted run loses the slices in
  flight and nothing already cached, and the next run owes what is
  still missing.
- **The driver is handed only what it needs** of the step's environment:
  what every child gets, and Modal's own variables (`MODAL_TOKEN_ID`,
  `MODAL_TOKEN_SECRET`, `MODAL_PROFILE`, `MODAL_CONFIG_PATH`,
  `MODAL_ENVIRONMENT`, `MODAL_SERVER_URL`; `datalib_env::Child::Modal`).
  A sync's runner hands a step none of those unless `DATALIB_CHILD_ENV`
  names them, so under a sync the sign-in is `~/.modal.toml`.
- **`DATALIB_MODAL_IMAGE`, `DATALIB_MODAL_GPU` and
  `DATALIB_MODAL_SANDBOXES`** change the image, the GPU and how many
  Sandboxes run at once, for measuring; the last wins over the
  config's.

Durable work, where a laptop that sleeps mid-backfill loses nothing,
would need a deployed Modal Function and its `spawn_map`, which the
JavaScript SDK cannot reach (#1815).

### What a run reports it spent

- **As it goes**, running totals the sync dashboard draws and `/metrics`
  exports: `modal_chunks_total`, and by GPU `modal_sandboxes_total`,
  `modal_sandboxes_failed_total`, `modal_sandbox_seconds_total` and
  `modal_cost_estimate_micro_usd_total` (`datalib_metrics`). A Sandbox is
  counted as it ends, failed attempts too, for its life from when Modal
  placed it, which is when Modal starts to bill; the wait for a GPU is
  not counted.
- **The estimate errs high on purpose.** It prices each Sandbox's life at
  Modal's list price for its GPU plus an assumed core and 2 GiB, at the
  Sandbox's rate, three times a Function's (`modal::estimated_micro_usd`):
  a T4 Sandbox comes to $0.78 an hour, of which the GPU is $0.59. The
  price table in `modal.rs` is read from modal.com/pricing and has to
  follow it. Modal's own bill is the one to trust.
- **The Manage screen's Modal spend column** sums the estimate over the
  runs the run store keeps, per `embed` step and for its group, with
  the total climbing run by run behind it and the latest run's Sandboxes
  and Sandbox time in its hover (`http/src/manage/spend.rs`). It hides
  while no row has spent anything.
- **The run ends with one line** on its Manage row's Status and in its
  log: "embedded 8192 chunks on Modal: 3 T4 Sandbox(es), 6 min 0 s of
  Sandbox time, about $0.08 at list price" (`modal::summary`). A failed
  run puts the same words in its error, since the row shows the error.

## What a person notices

- With the parameter set, a source's first embed takes minutes rather
  than hours and shows the same progress bar; its daily embeds run here
  as before.
- While it runs, the row's Status says "search index: embedding on
  Modal"; when it ends, the line above.
- A run over the threshold with no Modal sign-in fails and says how to
  sign in or raise the threshold (`modal::not_signed_in`). It does not
  quietly embed here, which would cost the hours the person chose not to
  spend.
- A search reads these vectors as any other. Vectors from a GPU and from
  a Mac differ in the last digits, as Metal's and the CPU's do.

## Measured on Modal

`gpu_check` with the module beside it, in the plain image of 0.54.1
plus cuBLAS from NVIDIA's `nvidia-cublas-cu12` wheel, over the TNG
fixture's 484 prompts (117,839 tokens), and the CPU over the first 352.
The module loaded on every GPU.

| device | prompts/s | tokens/s | $ per M tokens |
|---|---|---|---|
| Modal T4 | 41–43 | 10,000–10,500 | 0.016 |
| Modal A100 40 GB | 56.8 | 13,800 | 0.042 |
| Modal H100 | 87.8 | 21,400 | 0.051 |
| M-series Mac, Metal | 20.6–27.6 | 5,000–6,700 | — |
| Modal's x86 CPU, in the GPU container | 2.0–3.1 | 470–700 | — |

One prompt per call, the first call's start included; the price is the
GPU's list price per second over tokens per second, CPU and memory left
out.

**Agreement.** CUDA against Metal: worst cosine 0.99913, mean 0.99973,
as close as Metal is to the Mac's CPU (0.99936). CUDA against either CPU:
0.9983 on one 26-token prompt, 0.9993 or better on every other, the same
on all three GPUs. `gpu_check` holds CUDA to the CPU at 0.998.

**Several prompts per call.** embeddinggemma keeps no cache in
llama.cpp, so one call attends over every token in it, each prompt
masked from the others, and a call's time and scratch memory grow with
the square of its tokens. The embedder packs prompts in order into calls
of at most a count and a token budget (`Batching` in
`datalib/backend/embedder/src/lib.rs`), and a prompt's vector does not
depend on its neighbours (`embedder/tests/parity.rs`). Over the same
prompts, the first call left out:

| device | 1 per call | 8 per call, 2,048 tokens | 16 per call, 4,096 | 128 per call, 32,768 |
|---|---|---|---|---|
| T4 | 44.3/s | 51.0/s | 29.5/s | 6.9/s |
| A100 40 GB | 81.3/s | 146.5/s | 82.0/s | 26.4/s |
| H100 | 97.1/s | 145.7/s | 82.2/s | 25.1/s |
| Metal (M-series) | 25.6/s | 27.1/s | 23.2/s (8 per call) | 26.2/s (32 per call, 8,192) |

So CUDA takes 8 prompts in 2,048 tokens a call, and the CPU and Metal one
(`Batching::for_device`). The A100 and H100 tie, so at this size
something other than the GPU's arithmetic bounds them. CUDA against the
CPU did not move with the batching (worst cosine 0.9992 to 0.9995 over 64
prompts).

**Cost.** At 8 per call, a million tokens is $0.013 on a T4, $0.016 on
an A100 and $0.031 on an H100. A library of 710k chunks at some 560
tokens each is about 400M tokens: some $5 on T4s (9 GPU-hours). These
prompts average 243 tokens; a library's longer chunks share a call less,
and gain less. Splitting a run over many Sandboxes costs the same
GPU-seconds as one and divides the wait, plus each one's start.

**Through the step.** `datalib-step` 0.55.0 sent the TNG fixture's 74
owed `claude-api` chunks to one T4 Sandbox: 108.9 s, nearly all of it
the Sandbox's start on an account that had not pulled the image, about
$0.02 at list price. Against the fixture's own vectors the worst cosine
was 0.99966. Then 5,000 made-up prompts of about 2 KB came back whole
from two T4 Sandboxes, 4,096 in 132.8 s and 904 in 96.2 s, worst cosine
0.99833 against Metal on 50 of them.
