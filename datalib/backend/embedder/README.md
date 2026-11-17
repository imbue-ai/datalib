# `datalib_embedder`: text to vectors

The embedder runs the search model, embeddinggemma 300M (Q8_0, the file
`datalib-step pull-models` puts in place), through llama.cpp
(`third-party/llama_cpp`) behind a small C shim (`csrc/embedder.cpp`). It
is set up as qmd's node-llama-cpp set it up, so the vectors are the ones
qmd wrote; `tests/parity.rs` holds that against qmd itself.

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
- **A vector is not unit length.** The model's pooling leaves it about
  1,100 long, varying by input. Search compares by cosine, which reads
  only its direction; anything comparing by dot product must normalize
  first.
- **An input past the context is refused**; `fit` cuts it to the context,
  keeping its last token (EOS) last, and `embed_fitted` does that first.
  A chunk of dense text can be over the context though it is within the
  chunker's bytes.
- **The same input in the same call gives the same vector, to the bit**,
  on the CPU and on the best device. The inference cache keys a vector by
  its prompt alone and relies on this.

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

## Measured by hand

CI has no GPU, so these were measured with `gpu_check`
(`tools/gpu_check.rs`, built with the CUDA module by
`.github/workflows/cuda.yml`) on Modal's GPUs and on an M-series Mac,
over the TNG fixture's 484 prompts. The numbers and how they were taken
are in `docs/dev/plans/embed_on_modal.md` § "Measured on Modal".

- **CUDA comes as a module.** A gnu Linux build loads `libggml-cuda.so`
  from `lib/` beside the running binary when it is there and loads,
  and runs on the CPU otherwise, logging which and why
  (`cuda_module_report`). The module needs the host's driver and cuBLAS
  12 (`libcublas.so.12`, `libcublasLt.so.12`).
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
