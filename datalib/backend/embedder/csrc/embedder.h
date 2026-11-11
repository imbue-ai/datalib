// The part of llama.cpp the embedder calls, in plain C types, so the Rust
// side declares a handful of functions and no llama.cpp struct.

#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct dl_embedder dl_embedder;

// NULL on failure, with the reason in err. With use_gpu, the model and its
// work go to the GPU where this build and machine have one (Metal on a
// mac); without it, everything stays on the CPU.
dl_embedder *dl_embedder_open(const char *model_path, int32_t n_ctx, int32_t n_threads,
                              int32_t use_gpu, char *err, size_t err_len);

// Whether this build and machine offer a GPU at all.
int32_t dl_embedder_gpu_available(void);
void dl_embedder_close(dl_embedder *e);

// The device the model runs on: "CPU", or the GPU's name ("MTL0").
const char *dl_embedder_device(const dl_embedder *e);

int32_t dl_embedder_n_embd(const dl_embedder *e);
int32_t dl_embedder_n_ctx(const dl_embedder *e);

// The model's tokens for text, with its BOS and EOS added and special-token
// text read as plain text. Returns the count; when it exceeds cap, nothing
// past cap is written and the caller asks again with room for the count.
// Negative on failure.
int32_t dl_embedder_tokenize(const dl_embedder *e, const char *text, int32_t text_len,
                             int32_t *out, int32_t cap);

// The pooled embedding of n tokens, n_embd floats into out, evaluated
// `batch` tokens at a time; 0 or n or more is all at once. 0 on success.
int32_t dl_embedder_embed(dl_embedder *e, const int32_t *tokens, int32_t n, int32_t batch,
                          float *out, char *err, size_t err_len);

#ifdef __cplusplus
}
#endif
