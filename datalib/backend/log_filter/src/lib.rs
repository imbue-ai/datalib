//! The log filter every datalib process starts from when `RUST_LOG`
//! does not say otherwise.

/// Our own lines down to `debug` — the run store is cheap and rotates,
/// and a question about a sync is easier to answer with more than with
/// less — and the libraries that would talk over it kept to warnings.
/// `sqlx` matters most: at `debug` it logs every statement, including
/// the writes that put these lines in the store.
///
/// The step and the runner that stores its lines share this on purpose:
/// a step that filtered at `info` would never hand the runner a `debug`
/// line to keep.
pub const DEFAULT_LOG_FILTER: &str =
    "debug,sqlx=warn,hyper=warn,h2=warn,rustls=warn,notify=warn,html5ever=error";
