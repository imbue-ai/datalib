//! Text to vectors, in process: llama.cpp (`third-party/llama_cpp`) running
//! a GGUF embedding model, behind a small C shim (`csrc/embedder.cpp`). Set
//! up as qmd's node-llama-cpp set it up, so the vectors are the ones qmd
//! wrote; `tests/parity.rs` holds that against qmd itself.

use std::ffi::{c_char, CString};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

mod ffi {
    use std::ffi::c_char;

    #[repr(C)]
    pub struct Embedder {
        _private: [u8; 0],
    }

    extern "C" {
        pub fn dl_embedder_open(
            model_path: *const c_char,
            n_ctx: i32,
            n_threads: i32,
            use_gpu: i32,
            err: *mut c_char,
            err_len: usize,
        ) -> *mut Embedder;
        pub fn dl_embedder_load_module(
            path: *const c_char,
            err: *mut c_char,
            err_len: usize,
        ) -> i32;
        pub fn dl_embedder_gpu_available() -> i32;
        pub fn dl_embedder_close(e: *mut Embedder);
        pub fn dl_embedder_device(e: *const Embedder) -> *const c_char;
        pub fn dl_embedder_n_embd(e: *const Embedder) -> i32;
        pub fn dl_embedder_n_ctx(e: *const Embedder) -> i32;
        pub fn dl_embedder_tokenize(
            e: *const Embedder,
            text: *const c_char,
            text_len: i32,
            out: *mut i32,
            cap: i32,
        ) -> i32;
        pub fn dl_embedder_embed(
            e: *mut Embedder,
            tokens: *const i32,
            n: i32,
            batch: i32,
            out: *mut f32,
            err: *mut c_char,
            err_len: usize,
        ) -> i32;
    }
}

/// qmd's embedding context size (`LlamaCpp.EMBED_CONTEXT_SIZE` in its
/// `src/llm.ts`), which is also embeddinggemma's training context.
pub const CONTEXT_TOKENS: i32 = 2048;

/// Where the model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    /// The GPU where there is one (Metal on a mac; CUDA on gnu Linux, when
    /// [`cuda_module_report`] loaded the module), else the CPU.
    Best,
}

/// Whether this build and machine offer a GPU.
pub fn gpu_available() -> bool {
    unsafe { ffi::dl_embedder_gpu_available() != 0 }
}

/// Adds the ggml backend module at exactly `path` to the devices
/// [`Device::Best`] chooses from; the count of devices it offers.
pub fn load_gpu_module(path: &Path) -> Result<usize> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .context("a module path with a NUL in it")?;
    let mut err = [0 as c_char; 512];
    let n = unsafe { ffi::dl_embedder_load_module(c_path.as_ptr(), err.as_mut_ptr(), err.len()) };
    if n < 0 {
        bail!("{}", message(&err));
    }
    Ok(usize::try_from(n)?)
}

/// Where a gnu Linux build looks for the CUDA module: `lib/` beside the
/// running binary, as the release tarball lays it out. Never the loader's
/// search path, which would run whatever library it found first.
pub fn cuda_module_path(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.join("lib").join("libggml-cuda.so"))
}

/// What became of the CUDA module, decided once per process on the first
/// [`Device::Best`] open: `None` on a build that does not look for one
/// (macOS, musl).
pub fn cuda_module_report() -> Option<&'static str> {
    static REPORT: OnceLock<Option<String>> = OnceLock::new();
    REPORT
        .get_or_init(|| {
            if !cfg!(all(target_os = "linux", target_env = "gnu")) {
                return None;
            }
            let exe = std::env::current_exe().ok();
            let Some(path) = exe.as_deref().and_then(cuda_module_path) else {
                return Some("no CUDA module: the running binary's path is unknown".into());
            };
            Some(if !path.exists() {
                format!("no CUDA module at {}", path.display())
            } else {
                match load_gpu_module(&path) {
                    Ok(n) => format!("loaded {}: {n} CUDA device(s)", path.display()),
                    Err(e) => format!("could not load {}: {e:#}", path.display()),
                }
            })
        })
        .as_deref()
}

/// A loaded model and one context over it. Drop it before the process
/// exits: on Metal, ggml asserts at exit that no model is still loaded, and
/// aborts the process otherwise, so an embedder must not live in a static.
pub struct Embedder {
    raw: NonNull<ffi::Embedder>,
    dim: usize,
    context: usize,
}

// The context is used from one thread at a time: `embed` takes `&mut self`,
// and tokenizing reads only the model's vocabulary.
unsafe impl Send for Embedder {}

impl Embedder {
    /// `threads` of 0 is llama.cpp's own choice.
    pub fn open(model: &Path, device: Device, threads: u32) -> Result<Embedder> {
        if device == Device::Best {
            cuda_module_report();
        }
        let path = CString::new(model.as_os_str().as_encoded_bytes())
            .context("a model path with a NUL in it")?;
        let mut err = [0 as c_char; 256];
        let raw = unsafe {
            ffi::dl_embedder_open(
                path.as_ptr(),
                CONTEXT_TOKENS,
                i32::try_from(threads)?,
                i32::from(device == Device::Best),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            bail!("open {}: {}", model.display(), message(&err));
        };
        let dim = unsafe { ffi::dl_embedder_n_embd(raw.as_ptr()) };
        let context = unsafe { ffi::dl_embedder_n_ctx(raw.as_ptr()) };
        Ok(Embedder {
            raw,
            dim: usize::try_from(dim)?,
            context: usize::try_from(context)?,
        })
    }

    /// What the model runs on: `CPU`, or the GPU's name (`MTL0` on a mac,
    /// `CUDA0`).
    pub fn device(&self) -> String {
        let name = unsafe { std::ffi::CStr::from_ptr(ffi::dl_embedder_device(self.raw.as_ptr())) };
        name.to_string_lossy().into_owned()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// How many tokens one input may be, BOS and EOS included.
    pub fn context(&self) -> usize {
        self.context
    }

    /// The tokens `embed` would run for `text`, BOS and EOS included.
    pub fn tokenize(&self, text: &str) -> Result<Vec<i32>> {
        let len = i32::try_from(text.len()).context("a text over 2 GB")?;
        let mut out = vec![0i32; text.len() / 2 + 8];
        loop {
            let cap = i32::try_from(out.len())?;
            let n = unsafe {
                ffi::dl_embedder_tokenize(
                    self.raw.as_ptr(),
                    text.as_ptr().cast(),
                    len,
                    out.as_mut_ptr(),
                    cap,
                )
            };
            if n < 0 {
                bail!("llama.cpp could not tokenize the text");
            }
            let n = n as usize;
            if n <= out.len() {
                out.truncate(n);
                return Ok(out);
            }
            out.resize(n, 0);
        }
    }

    /// The embedding of `text`, as the model's pooling leaves it (not unit
    /// length). Fails on a text longer than the context.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let tokens = self.tokenize(text)?;
        self.embed_tokens(&tokens)
    }

    /// [`Self::embed`], with a text longer than the context cut to fit
    /// first, as qmd does; and whether it was cut.
    pub fn embed_fitted(&mut self, text: &str) -> Result<(Vec<f32>, bool)> {
        let tokens = self.tokenize(text)?;
        let (tokens, cut) = fit(tokens, self.context);
        Ok((self.embed_tokens(&tokens)?, cut))
    }

    pub fn embed_tokens(&mut self, tokens: &[i32]) -> Result<Vec<f32>> {
        self.embed_in_batches(tokens, 0)
    }

    /// Evaluates `tokens` `batch` at a time (0 is all at once), as
    /// node-llama-cpp did for qmd. Only for showing what that did: a model
    /// that attends both ways needs its whole input in one batch.
    pub fn embed_in_batches(&mut self, tokens: &[i32], batch: usize) -> Result<Vec<f32>> {
        let mut out = vec![0f32; self.dim];
        let mut err = [0 as c_char; 256];
        let rc = unsafe {
            ffi::dl_embedder_embed(
                self.raw.as_ptr(),
                tokens.as_ptr(),
                i32::try_from(tokens.len())?,
                i32::try_from(batch)?,
                out.as_mut_ptr(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc != 0 {
            bail!("embed {} tokens: {}", tokens.len(), message(&err));
        }
        Ok(out)
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe { ffi::dl_embedder_close(self.raw.as_ptr()) }
    }
}

fn message(err: &[c_char]) -> String {
    let bytes: Vec<u8> = err
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `tokens` cut to `context`, its last token (the end of input) kept last,
/// and whether anything was cut.
pub fn fit(mut tokens: Vec<i32>, context: usize) -> (Vec<i32>, bool) {
    if tokens.len() <= context || context == 0 {
        return (tokens, false);
    }
    let last = tokens[tokens.len() - 1];
    tokens.truncate(context - 1);
    tokens.push(last);
    (tokens, true)
}

#[cfg(test)]
mod tests {
    use super::{cuda_module_path, fit, load_gpu_module};
    use std::path::Path;

    #[test]
    fn the_cuda_module_is_looked_for_in_lib_beside_the_binary() {
        assert_eq!(
            cuda_module_path(Path::new("/opt/datalib/datalib-step")).unwrap(),
            Path::new("/opt/datalib/lib/libggml-cuda.so")
        );
    }

    /// A module that will not load says why, in the loader's words.
    #[test]
    fn a_module_that_will_not_load_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("libggml-cuda.so");
        let err = load_gpu_module(&missing).unwrap_err().to_string();
        assert!(err.contains("libggml-cuda.so"), "{err}");

        let not_a_library = dir.path().join("not-a-library.so");
        std::fs::write(&not_a_library, b"not an ELF file").unwrap();
        let err = load_gpu_module(&not_a_library).unwrap_err().to_string();
        assert!(!err.is_empty());
    }

    #[test]
    fn a_long_input_is_cut_to_the_context_keeping_its_end() {
        assert_eq!(
            fit(vec![2, 10, 11, 12, 1], 5),
            (vec![2, 10, 11, 12, 1], false)
        );
        assert_eq!(
            fit(vec![2, 10, 11, 12, 13, 1], 4),
            (vec![2, 10, 11, 1], true)
        );
    }
}
