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
// mac); without it, everything stays on the CPU. Each input may be n_ctx
// tokens; one call takes up to n_seq_max inputs and n_tokens tokens in all
// (raised to n_ctx when less).
dl_embedder *dl_embedder_open(const char *model_path, int32_t n_ctx, int32_t n_threads,
                              int32_t use_gpu, int32_t n_seq_max, int32_t n_tokens, char *err,
                              size_t err_len);

// The device dl_embedder_open would choose: "CPU", or the GPU's name.
const char *dl_embedder_device_for(int32_t use_gpu);

// Loads a ggml backend module (libggml-cuda.so) from exactly path, before
// any embedder opens. The count of devices it offers, or -1 with the reason
// (the loader's own message) in err.
int32_t dl_embedder_load_module(const char *path, char *err, size_t err_len);

// Whether this build and machine offer a GPU at all.
// 1 when the dynamic loader can load `name` (a library's soname), else 0.
int32_t dl_embedder_can_load(const char *name);

int32_t dl_embedder_gpu_available(void);
void dl_embedder_close(dl_embedder *e);

// The device the model runs on: "CPU", or the GPU's name ("MTL0").
const char *dl_embedder_device(const dl_embedder *e);

int32_t dl_embedder_n_embd(const dl_embedder *e);
// Tokens one input may be.
int32_t dl_embedder_n_ctx(const dl_embedder *e);
int32_t dl_embedder_n_seq_max(const dl_embedder *e);
int32_t dl_embedder_n_tokens(const dl_embedder *e);
// Whether llama.cpp keeps a cache (KV or other memory) for this model.
int32_t dl_embedder_keeps_cache(const dl_embedder *e);

// The model's tokens for text, with its BOS and EOS added and special-token
// text read as plain text. Returns the count; when it exceeds cap, nothing
// past cap is written and the caller asks again with room for the count.
// Negative on failure.
int32_t dl_embedder_tokenize(const dl_embedder *e, const char *text, int32_t text_len,
                             int32_t *out, int32_t cap);

// The model's tokenizer alone: its vocabulary read from the file, without
// its weights or a context. NULL on failure, with the reason in err.
typedef struct dl_tokenizer dl_tokenizer;
dl_tokenizer *dl_tokenizer_open(const char *model_path, char *err, size_t err_len);
void dl_tokenizer_close(dl_tokenizer *t);

// Where each of text's tokens ends, as a byte offset into text, with no
// BOS or EOS and special-token text read as plain text. Returns the count;
// when it exceeds cap, nothing past cap is written and the caller asks
// again with room for the count. -2 when a token's piece cannot be lined
// up with the text; -1 on any other failure.
int32_t dl_tokenizer_ends(const dl_tokenizer *t, const char *text, int32_t text_len,
                          int32_t *out, int32_t cap);

// The pooled embedding of n tokens, n_embd floats into out, evaluated
// `batch` tokens at a time; 0 or n or more is all at once. 0 on success.
int32_t dl_embedder_embed(dl_embedder *e, const int32_t *tokens, int32_t n, int32_t batch,
                          float *out, char *err, size_t err_len);

// The pooled embeddings of n_seqs inputs in one call, n_embd floats each
// into out, in order. tokens holds the inputs end to end, lens[s] long
// each. 0 on success.
int32_t dl_embedder_embed_seqs(dl_embedder *e, const int32_t *tokens, const int32_t *lens,
                               int32_t n_seqs, float *out, char *err, size_t err_len);

#ifdef __cplusplus
}
#endif
