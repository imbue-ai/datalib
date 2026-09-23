//! `status_line!` and the process-wide [`MultiProgress`] it writes above.
//! `datalib_obs::init` publishes the one it builds here; see BUILD.bazel
//! for why this is not part of `datalib_obs`.

use std::sync::{Arc, OnceLock};

use indicatif::MultiProgress;

static SHARED_MULTI: OnceLock<Arc<MultiProgress>> = OnceLock::new();

/// The first call wins; later ones are no-ops, which matters only to
/// tests that initialize observability more than once per process.
pub fn set_shared_multi(multi: Arc<MultiProgress>) {
    let _ = SHARED_MULTI.set(multi);
}

/// The process-wide [`MultiProgress`] `datalib_obs::init` published. Returns
/// `None` before `init` runs (e.g. in unit tests that don't initialize
/// observability) — callers should fall back to a local `MultiProgress`
/// or simply skip rendering bars in that case.
pub fn shared_multi() -> Option<Arc<MultiProgress>> {
    SHARED_MULTI.get().cloned()
}

/// Write a one-line status message that coexists with the indicatif
/// progress bars. Routes through the shared `MultiProgress::println`
/// when bars can actually draw (stderr is a TTY), which suspends draws
/// across the write so the line lands above the bar block instead of
/// overprinting it. Falls back to `eprintln!` before `init` runs, in
/// tests that skip observability, and — crucially — whenever the draw
/// target is hidden (stderr piped/redirected): `MultiProgress::println`
/// is documented to *do nothing* in that case, which once swallowed
/// every status line (including the sync phase markers the http worker
/// scrapes) when the pipeline ran as a child process. Same
/// `format!` argument grammar as `eprintln!`.
#[macro_export]
macro_rules! status_line {
    ($($arg:tt)*) => {{
        let __msg = ::std::format!($($arg)*);
        match $crate::shared_multi() {
            // `MultiProgress::println` is documented to do *nothing*
            // when the draw target is hidden — which is exactly the
            // off-TTY case (stderr piped, e.g. spawned by the http
            // worker). Only route through it when it will actually
            // draw; otherwise fall through to raw stderr so status
            // lines (incl. the sync phase markers) reach the pipe.
            Some(mp) if !mp.is_hidden() => { let _ = mp.println(&__msg); }
            // Pre-`init` (tracing not up yet), hidden draw target, and
            // out-of-band fallback. The lint exception is part of the
            // macro's contract.
            _ => {
                #[allow(clippy::disallowed_macros)]
                { ::std::eprintln!("{}", __msg); }
            }
        }
    }}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `status_line!` must not route through `MultiProgress::println`
    /// when the draw target is hidden — indicatif documents that call
    /// as a no-op, so taking that branch silently swallows the line.
    /// Off a TTY (child process with piped stderr, CI, `bazel test`)
    /// a stderr-targeted `MultiProgress` *is* hidden, so the macro's
    /// `!mp.is_hidden()` guard is what keeps the sync phase markers
    /// flowing to the http worker's pipe.
    #[test]
    fn stderr_multi_is_hidden_off_tty() {
        if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            // Interactive run: the premise doesn't hold; nothing to check.
            return;
        }
        // Not the process-wide MP — this test constructs a throwaway
        // one purely to observe indicatif's TTY detection.
        #[allow(clippy::disallowed_methods)]
        let mp = MultiProgress::new();
        assert!(
            mp.is_hidden(),
            "stderr-targeted MultiProgress should be hidden off-TTY; \
             if this changes, revisit the status_line! routing guard"
        );
    }

    /// Explicitly-hidden target, TTY-independent: `println` reports Ok
    /// while drawing nothing, which is exactly why the macro must not
    /// treat `Some(mp)` alone as "safe to route through".
    #[test]
    fn hidden_multi_println_is_a_silent_no_op() {
        let mp = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        assert!(mp.is_hidden());
        assert!(mp.println("dropped on the floor").is_ok());
    }
}
