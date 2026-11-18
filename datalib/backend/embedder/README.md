# `datalib_embedder`: text to vectors

The embedder runs the search model, embeddinggemma 300M (Q8_0, the file
`datalib-step pull-models` puts in place), through llama.cpp
(`third-party/llama_cpp`) behind a small C shim (`csrc/embedder.cpp`). It
is set up as qmd's node-llama-cpp set it up, so the vectors are the ones
qmd wrote; `tests/parity.rs` holds that against a golden of qmd's answers.

**The facts below are tests.** `//datalib/backend/embedder:embedding_facts`
(`tests/embedding_facts.rs`) checks each one, in this order. Run it after
a llama.cpp or model bump: a fact that moved fails by name. Fix the fact
here, then the test, then whatever leaned on it. What CI cannot see,
having no GPU, is in § "Measured by hand".

## The model

- **A vector is 768 numbers, and an input up to 2,048 tokens**
  (`CONTEXT_TOKENS`, the model's training context and qmd's).
- **Every input starts with BOS and ends with EOS**, which the tokenizer
  adds.
- **Text that spells a special token is plain text**: `<eos>` written in a
  message is not the end of the input.
- **The tokenizer alone counts as the model does.** `Tokenizer` reads only
  the vocabulary from the model file, without the weights, and gives each
  token's end as a byte offset into the text; the offsets end at the
  text's end, and there are two fewer than the model runs (no BOS, no
  EOS). A token's piece spells a `▁` (U+2581, the mark SentencePiece
  reads a space as) back as a space, so the offsets come from lining the
  pieces up against the text, not from adding their lengths. The chunker
  counts its budget with it.
- **A vector is not unit length.** The model's pooling leaves it about
  1,100 long, varying by input. Search compares by cosine, which reads
  only its direction; anything comparing by dot product must normalize
  first.
- **An input past the context is refused**; `fit` cuts it to the context,
  keeping its last token (EOS) last, and `embed_fitted` does that first.
  The chunker's budget is counted in these tokens, so a chunk is well
  inside the context; only an unusually long title or envelope in front
  of it can still push a prompt over.
- **The same input in the same call gives the same vector, to the bit**,
  on the CPU and on the best device. The inference cache keys a vector by
  its prompt alone and relies on this.
- **The thread count does not change a vector**, to the bit: 1, 3 and 4
  CPU threads agree. Tests run the embedder on fewer threads than a
  sync does (§ "Threads"), and their vectors are a sync's.

## One call

- **llama.cpp keeps no cache for this model.** So one call attends over
  every token in it, each prompt masked from the others, and a call's
  time and scratch memory grow with the square of its tokens.
  `Batching { prompts, tokens }` caps both: prompts are packed in order
  into calls of at most `prompts` inputs and `tokens` tokens, and a
  budget of one context (2,048) costs no more than one long prompt alone.
- **A prompt takes nothing from what shares its call**: beside two
  different neighbours of the same length, it comes out the same to the
  bit.
- **A prompt sharing a call gets its vector alone, to rounding**: cosine
  over 0.999, about 0.9995 at worst on arm64's CPU. The CPU picks its
  kernels by the call's token count, so the same prompt in a larger call
  rounds differently, though nothing of its neighbours reaches it.

## Devices

- **On a mac, the best device is Metal** (`MTL0`).
- **The best device agrees with the CPU** to a cosine over 0.999.

## Threads

- **A caller that passes 0 threads gets `DATALIB_EMBED_THREADS`**, and
  llama.cpp's own choice where that is unset: four, on any machine. The
  step's `embed` and the search applet pass 0. A value that is not a
  count is an error, not a fallback.
- **llama.cpp's threads wait for each other spinning.** ggml is built
  without OpenMP, and `ggml_barrier` busy-waits; a model's forward pass
  crosses it hundreds of times. On a machine with more runnable threads
  than cores, each crossing waits for a thread the scheduler has set
  aside. Measured on a 4-vCPU VM, embedding 60 prompts:

  | threads | idle | beside 3 busy processes | beside 6 |
  | --- | --- | --- | --- |
  | 4 (llama.cpp's) | 25.7 s | 166 s | 286 s |
  | 2 | 43.5 s | 64 s | 158 s |
  | 1 | 71.7 s | 72 s | 125 s |

  `//datalib/backend/applets:applet_unittests` took 21 s alone at four
  threads and timed out at 300 s beside 3 busy processes; at two it took
  24 s alone and 30 to 37 s beside 3 or 6. So every test runs the
  embedder on two (`.bazelrc`'s `--test_env`), and the steps and
  applets a test starts inherit it (`datalib_env::hands_down`).

## Measured by hand

CI has no GPU, so these were measured with `gpu_check`
(`tools/gpu_check.rs`, built with the CUDA module by
`.github/workflows/cuda.yml`) on Modal's GPUs and on an M-series Mac,
over the TNG fixture's 484 prompts. The numbers and how they were taken
are in `docs/dev/plans/embed_on_modal.md` § "Measured on Modal".

- **CUDA comes as a module.** A gnu Linux build loads `libggml-cuda.so`
  from `lib/` beside the running binary when it is there (the `-cuda`
  image). Otherwise, where NVIDIA's driver library `libcuda.so.1` loads,
  it loads the module the release tarball's `runtime.manifest` names,
  fetched into `~/.cache/datalib/cuda/` on first use
  (`datalib_runtime::cuda_module`, `docs/dev/runtime_fetch.md` § "The
  CUDA module"). It runs on the CPU when neither gives a module that
  loads, logging which and why (`cuda_module_report`). The module needs
  the host's driver and cuBLAS 12 (`libcublas.so.12`,
  `libcublasLt.so.12`).
- **CUDA agrees with Metal** as closely as any two devices do (worst
  cosine 0.9991), and with the CPU to 0.9983 at worst, on one 26-token
  prompt; `gpu_check` holds it to 0.998.
- **Sharing a call pays on CUDA, not on Metal.** 8 prompts in 2,048
  tokens ran 15% (T4) to 80% (A100) faster than one per call; any larger
  token budget was slower on every GPU. On Metal, sharing 2,048 tokens
  gained nothing and cost no memory (2.3 GB peak); 4,096 tokens took
  4.5 GB and 8,192 took 8 to 9 GB, neither faster. So
  `Batching::for_device` gives the CPU and Metal one prompt per call, and
  any other GPU 8 in 2,048 tokens.
