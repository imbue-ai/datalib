// The part of llama.cpp the embedder calls, in plain C types, so the Rust
// side declares a handful of functions and no llama.cpp struct.

#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct dl_embedder dl_embedder;

// NULL on failure, with the reason in err.
dl_embedder *dl_embedder_open(const char *model_path, int32_t n_ctx, int32_t n_threads,
                              char *err, size_t err_len);
void dl_embedder_close(dl_embedder *e);

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
