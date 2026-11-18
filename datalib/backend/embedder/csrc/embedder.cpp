// One embedding context over one model, set up as node-llama-cpp 3.20.0
// sets up qmd's (llama/addon/AddonContext.cpp there): embeddings on, the
// model's own pooling, the sliding-window cache not kept full, and the
// whole input in one batch, which a model that attends both ways needs.

#include "embedder.h"

#include <dlfcn.h>

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <set>
#include <vector>

#include "llama.h"

struct dl_embedder {
    // Held by every call that reads the model, and at exit.
    mutable std::mutex mu;
    // Both NULL once freed at exit.
    llama_model *model;
    llama_context *ctx;
    const char *device;
};

struct dl_tokenizer {
    llama_model *model;
};

namespace {

void set_err(char *err, size_t err_len, const char *msg) {
    if (err && err_len) {
        std::snprintf(err, err_len, "%s", msg);
    }
}

// llama.cpp logs every load to stderr; a step's stderr is its log.
void quiet(ggml_log_level level, const char *text, void *) {
    if (level == GGML_LOG_LEVEL_ERROR) {
        std::fputs(text, stderr);
    }
}

void init_once() {
    static std::once_flag once;
    std::call_once(once, [] {
        llama_log_set(quiet, nullptr);
        llama_backend_init();
    });
}

// Every embedder not yet closed. Never destroyed, since it is read
// while the process's statics are being torn down.
struct Open {
    std::mutex mu;
    std::set<dl_embedder *> all;
};

Open &open_embedders() {
    static Open *open = new Open;
    return *open;
}

void free_model(dl_embedder *e) {
    llama_free(e->ctx);
    llama_model_free(e->model);
    e->ctx = nullptr;
    e->model = nullptr;
}

// ggml's Metal device is a C++ static whose destructor asserts that no
// model's buffers are left on it, and aborts the process otherwise. Statics
// are torn down in the reverse of the order they were built, so this one,
// built when the first model has loaded, frees any model still loaded at
// exit before that device goes. (A static rather than atexit, which glibc
// keeps in libc_nonshared.a, where the CUDA build's link does not look.)
struct FreeModelsLeftAtExit {
    ~FreeModelsLeftAtExit() {
        Open &open = open_embedders();
        std::lock_guard<std::mutex> all(open.mu);
        for (dl_embedder *e : open.all) {
            std::lock_guard<std::mutex> one(e->mu);
            if (e->model) free_model(e);
        }
    }
};

void track(dl_embedder *e) {
    Open &open = open_embedders();
    {
        std::lock_guard<std::mutex> all(open.mu);
        open.all.insert(e);
    }
    static FreeModelsLeftAtExit at_exit;
}

void untrack(dl_embedder *e) {
    Open &open = open_embedders();
    std::lock_guard<std::mutex> all(open.mu);
    open.all.erase(e);
}

const char kFreedAtExit[] = "the process is exiting; its model is freed";

}  // namespace

extern "C" {

// The one GPU to run on, or NULL for none.
ggml_backend_dev_t gpu() {
    ggml_backend_dev_t dev = ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU);
    return dev ? dev : ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_IGPU);
}

int32_t dl_embedder_load_module(const char *path, char *err, size_t err_len) {
    // ggml_backend_load logs a failure without saying why; the loader does.
    // Asked first, a module that will not load never starts ggml.
    void *probe = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!probe) {
        const char *why = dlerror();
        set_err(err, err_len, why ? why : "dlopen failed");
        return -1;
    }
    dlclose(probe);
    init_once();
    ggml_backend_reg_t reg = ggml_backend_load(path);
    if (!reg) {
        set_err(err, err_len, "loaded, but ggml did not take it as a backend");
        return -1;
    }
    return (int32_t)ggml_backend_reg_dev_count(reg);
}

int32_t dl_embedder_can_load(const char *name) {
    void *handle = dlopen(name, RTLD_LAZY | RTLD_LOCAL);
    if (!handle) return 0;
    dlclose(handle);
    return 1;
}

int32_t dl_embedder_gpu_available(void) {
    init_once();
    return gpu() ? 1 : 0;
}

const char *dl_embedder_device_for(int32_t use_gpu) {
    init_once();
    ggml_backend_dev_t dev = use_gpu ? gpu() : nullptr;
    return dev ? ggml_backend_dev_name(dev) : "CPU";
}

dl_embedder *dl_embedder_open(const char *model_path, int32_t n_ctx, int32_t n_threads,
                              int32_t use_gpu, int32_t n_seq_max, int32_t n_tokens, char *err,
                              size_t err_len) {
    init_once();
    if (n_seq_max < 1) n_seq_max = 1;
    if (n_tokens < n_ctx) n_tokens = n_ctx;
    llama_model_params mp = llama_model_default_params();
    ggml_backend_dev_t dev = use_gpu ? gpu() : nullptr;
    ggml_backend_dev_t devices[] = {dev, nullptr};
    mp.devices = devices;
    mp.n_gpu_layers = dev ? -1 : 0;
    llama_model *model = llama_model_load_from_file(model_path, mp);
    if (!model) {
        set_err(err, err_len, "llama.cpp could not load the model");
        return nullptr;
    }
    llama_context_params cp = llama_context_default_params();
    // Each input has n_ctx positions of its own. The model keeps no cache,
    // so one call attends over every token in it, inputs masked from each
    // other: its cost grows with the square of n_tokens, not of n_ctx.
    cp.n_ctx = n_ctx * n_seq_max;
    cp.n_batch = n_tokens;
    cp.n_ubatch = n_tokens;
    cp.n_seq_max = n_seq_max;
    cp.embeddings = true;
    cp.swa_full = false;
    cp.no_perf = true;
    // Otherwise llama.cpp sends a large batch's matrix products to the GPU
    // even with every layer on the CPU.
    cp.op_offload = dev != nullptr;
    if (n_threads > 0) {
        cp.n_threads = n_threads;
        cp.n_threads_batch = n_threads;
    }
    llama_context *ctx = llama_init_from_model(model, cp);
    if (!ctx) {
        llama_model_free(model);
        set_err(err, err_len, "llama.cpp could not make an embedding context");
        return nullptr;
    }
    if (llama_pooling_type(ctx) == LLAMA_POOLING_TYPE_NONE) {
        llama_free(ctx);
        llama_model_free(model);
        set_err(err, err_len, "the model pools nothing: not an embedding model");
        return nullptr;
    }
    auto *e = new dl_embedder{{}, model, ctx, dev ? ggml_backend_dev_name(dev) : "CPU"};
    track(e);
    return e;
}

void dl_embedder_close(dl_embedder *e) {
    if (!e) return;
    untrack(e);
    {
        std::lock_guard<std::mutex> one(e->mu);
        if (e->model) free_model(e);
    }
    delete e;
}

const char *dl_embedder_device(const dl_embedder *e) { return e->device; }

int32_t dl_embedder_n_embd(const dl_embedder *e) {
    std::lock_guard<std::mutex> one(e->mu);
    return e->model ? llama_model_n_embd_out(e->model) : 0;
}

int32_t dl_embedder_n_ctx(const dl_embedder *e) {
    std::lock_guard<std::mutex> one(e->mu);
    return e->model ? (int32_t)llama_n_ctx_seq(e->ctx) : 0;
}

int32_t dl_embedder_n_seq_max(const dl_embedder *e) {
    std::lock_guard<std::mutex> one(e->mu);
    return e->model ? (int32_t)llama_n_seq_max(e->ctx) : 0;
}

int32_t dl_embedder_keeps_cache(const dl_embedder *e) {
    std::lock_guard<std::mutex> one(e->mu);
    return e->model ? llama_get_memory(e->ctx) != nullptr : 0;
}

int32_t dl_embedder_n_tokens(const dl_embedder *e) {
    std::lock_guard<std::mutex> one(e->mu);
    return e->model ? (int32_t)llama_n_ubatch(e->ctx) : 0;
}

dl_tokenizer *dl_tokenizer_open(const char *model_path, char *err, size_t err_len) {
    init_once();
    llama_model_params mp = llama_model_default_params();
    mp.vocab_only = true;
    mp.n_gpu_layers = 0;
    llama_model *model = llama_model_load_from_file(model_path, mp);
    if (!model) {
        set_err(err, err_len, "llama.cpp could not read the model's vocabulary");
        return nullptr;
    }
    return new dl_tokenizer{model};
}

void dl_tokenizer_close(dl_tokenizer *t) {
    if (!t) return;
    llama_model_free(t->model);
    delete t;
}

int32_t dl_tokenizer_ends(const dl_tokenizer *t, const char *text, int32_t text_len,
                          int32_t *out, int32_t cap) {
    const llama_vocab *vocab = llama_model_get_vocab(t->model);
    int32_t n = llama_tokenize(vocab, text, text_len, nullptr, 0, false, false);
    if (n == INT32_MIN) return -1;
    n = n < 0 ? -n : n;
    if (n > cap) return n;
    std::vector<llama_token> tokens(n);
    if (n > 0 && llama_tokenize(vocab, text, text_len, tokens.data(), n, false, false) != n) {
        return -1;
    }
    // A piece spells its token as the model reads it, so it is lined up
    // against the text rather than measured: the tokenizer reads a space
    // as U+2581 (the "▁" SentencePiece marks one with), and a piece spells
    // that back as a space, whether the text had a space or a literal ▁.
    static const char kSpaceMark[] = "\xE2\x96\x81";
    int32_t at = 0;
    char piece[256];
    for (int32_t i = 0; i < n; i++) {
        int32_t len = llama_token_to_piece(vocab, tokens[i], piece, sizeof piece, 0, false);
        if (len < 0) return -1;
        for (int32_t k = 0; k < len; k++) {
            if (at < text_len && text[at] == piece[k]) {
                at += 1;
            } else if (piece[k] == ' ' && at + 3 <= text_len &&
                       std::memcmp(text + at, kSpaceMark, 3) == 0) {
                at += 3;
            } else {
                return -2;
            }
        }
        out[i] = at;
    }
    return n;
}

int32_t dl_embedder_tokenize(const dl_embedder *e, const char *text, int32_t text_len,
                             int32_t *out, int32_t cap) {
    std::lock_guard<std::mutex> one(e->mu);
    if (!e->model) return -1;
    const llama_vocab *vocab = llama_model_get_vocab(e->model);
    int32_t n = llama_tokenize(vocab, text, text_len, out, cap, true, false);
    // llama.cpp answers "too small" with the negated count.
    return n < 0 && n != INT32_MIN ? -n : n;
}

int32_t dl_embedder_embed(dl_embedder *e, const int32_t *tokens, int32_t n, int32_t batch,
                          float *out, char *err, size_t err_len) {
    std::lock_guard<std::mutex> one(e->mu);
    if (!e->model) {
        set_err(err, err_len, kFreedAtExit);
        return -1;
    }
    if (n <= 0 || n > (int32_t)llama_n_ctx_seq(e->ctx)) {
        set_err(err, err_len, "the input is empty or longer than the context");
        return -1;
    }
    if (batch <= 0 || batch > n) batch = n;
    llama_memory_clear(llama_get_memory(e->ctx), true);
    const bool encoder_only = llama_model_has_encoder(e->model) && !llama_model_has_decoder(e->model);
    for (int32_t from = 0; from < n; from += batch) {
        const int32_t len = std::min(batch, n - from);
        llama_batch b = llama_batch_init(len, 0, 1);
        for (int32_t i = 0; i < len; i++) {
            b.token[i] = tokens[from + i];
            b.pos[i] = from + i;
            b.n_seq_id[i] = 1;
            b.seq_id[i][0] = 0;
            b.logits[i] = 1;
        }
        b.n_tokens = len;
        const int32_t rc = encoder_only ? llama_encode(e->ctx, b) : llama_decode(e->ctx, b);
        llama_batch_free(b);
        if (rc != 0) {
            set_err(err, err_len, "llama.cpp failed to evaluate the input");
            return -1;
        }
    }
    const float *pooled = llama_get_embeddings_seq(e->ctx, 0);
    if (!pooled) {
        set_err(err, err_len, "llama.cpp returned no pooled embedding");
        return -1;
    }
    std::memcpy(out, pooled, sizeof(float) * (size_t)llama_model_n_embd_out(e->model));
    return 0;
}

int32_t dl_embedder_embed_seqs(dl_embedder *e, const int32_t *tokens, const int32_t *lens,
                               int32_t n_seqs, float *out, char *err, size_t err_len) {
    std::lock_guard<std::mutex> one(e->mu);
    if (!e->model) {
        set_err(err, err_len, kFreedAtExit);
        return -1;
    }
    if (n_seqs <= 0 || n_seqs > (int32_t)llama_n_seq_max(e->ctx)) {
        set_err(err, err_len, "no inputs, or more than the context takes at once");
        return -1;
    }
    int32_t total = 0;
    for (int32_t s = 0; s < n_seqs; s++) {
        if (lens[s] <= 0 || lens[s] > (int32_t)llama_n_ctx_seq(e->ctx)) {
            set_err(err, err_len, "an input is empty or longer than the context");
            return -1;
        }
        total += lens[s];
    }
    if (total > (int32_t)llama_n_ubatch(e->ctx)) {
        set_err(err, err_len, "the inputs are more tokens than one call takes");
        return -1;
    }
    llama_memory_clear(llama_get_memory(e->ctx), true);
    llama_batch b = llama_batch_init(total, 0, 1);
    int32_t i = 0;
    for (int32_t s = 0; s < n_seqs; s++) {
        for (int32_t p = 0; p < lens[s]; p++, i++) {
            b.token[i] = tokens[i];
            b.pos[i] = p;
            b.n_seq_id[i] = 1;
            b.seq_id[i][0] = s;
            b.logits[i] = 1;
        }
    }
    b.n_tokens = total;
    const bool encoder_only = llama_model_has_encoder(e->model) && !llama_model_has_decoder(e->model);
    const int32_t rc = encoder_only ? llama_encode(e->ctx, b) : llama_decode(e->ctx, b);
    llama_batch_free(b);
    if (rc != 0) {
        set_err(err, err_len, "llama.cpp failed to evaluate the inputs");
        return -1;
    }
    const size_t n_embd = (size_t)llama_model_n_embd_out(e->model);
    for (int32_t s = 0; s < n_seqs; s++) {
        const float *pooled = llama_get_embeddings_seq(e->ctx, s);
        if (!pooled) {
            set_err(err, err_len, "llama.cpp returned no pooled embedding");
            return -1;
        }
        std::memcpy(out + (size_t)s * n_embd, pooled, sizeof(float) * n_embd);
    }
    return 0;
}

}  // extern "C"
