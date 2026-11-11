// One embedding context over one model, set up as node-llama-cpp 3.20.0
// sets up qmd's (llama/addon/AddonContext.cpp there): embeddings on, the
// model's own pooling, the sliding-window cache not kept full, and the
// whole input in one batch, which a model that attends both ways needs.

#include "embedder.h"

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <mutex>

#include "llama.h"

struct dl_embedder {
    llama_model *model;
    llama_context *ctx;
    const char *device;
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

}  // namespace

extern "C" {

// The one GPU to run on, or NULL for none.
ggml_backend_dev_t gpu() {
    ggml_backend_dev_t dev = ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU);
    return dev ? dev : ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_IGPU);
}

int32_t dl_embedder_gpu_available(void) {
    init_once();
    return gpu() ? 1 : 0;
}

dl_embedder *dl_embedder_open(const char *model_path, int32_t n_ctx, int32_t n_threads,
                              int32_t use_gpu, char *err, size_t err_len) {
    init_once();
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
    cp.n_ctx = n_ctx;
    cp.n_batch = n_ctx;
    cp.n_ubatch = n_ctx;
    cp.n_seq_max = 1;
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
    return new dl_embedder{model, ctx, dev ? ggml_backend_dev_name(dev) : "CPU"};
}

void dl_embedder_close(dl_embedder *e) {
    if (!e) return;
    llama_free(e->ctx);
    llama_model_free(e->model);
    delete e;
}

const char *dl_embedder_device(const dl_embedder *e) { return e->device; }

int32_t dl_embedder_n_embd(const dl_embedder *e) { return llama_model_n_embd_out(e->model); }

int32_t dl_embedder_n_ctx(const dl_embedder *e) { return (int32_t)llama_n_ctx(e->ctx); }

int32_t dl_embedder_tokenize(const dl_embedder *e, const char *text, int32_t text_len,
                             int32_t *out, int32_t cap) {
    const llama_vocab *vocab = llama_model_get_vocab(e->model);
    int32_t n = llama_tokenize(vocab, text, text_len, out, cap, true, false);
    // llama.cpp answers "too small" with the negated count.
    return n < 0 && n != INT32_MIN ? -n : n;
}

int32_t dl_embedder_embed(dl_embedder *e, const int32_t *tokens, int32_t n, int32_t batch,
                          float *out, char *err, size_t err_len) {
    if (n <= 0 || n > (int32_t)llama_n_ctx(e->ctx)) {
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
    std::memcpy(out, pooled, sizeof(float) * (size_t)dl_embedder_n_embd(e));
    return 0;
}

}  // extern "C"
