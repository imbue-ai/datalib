//! The server's own log. Every `tracing` event goes two ways: to stderr
//! for whoever started the process, and to the run store for the app —
//! the same `log` table the runner fills, so the Manage screen reads
//! both through one endpoint.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};

use datalib_runs::{Process, ProcessLogWriter, Retention, StoreLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static PROCESS_ID: OnceLock<String> = OnceLock::new();
/// Weak like the subscriber's handle: the writer's drop is its final
/// flush, and a strong handle here would never let it happen.
static WRITER: OnceLock<Weak<ProcessLogWriter>> = OnceLock::new();

/// Which of the store's `processes` this server writes under, once
/// [`init`] has started the writer; `None` before, or when the store
/// could not be opened.
pub fn process_id() -> Option<String> {
    PROCESS_ID.get().cloned()
}

/// The store writer, for rows that do not come through `tracing`: what
/// a page of the app reports about itself. `None` before [`init`], when
/// the store could not be opened, or once the writer has been dropped.
pub fn writer() -> Option<Arc<ProcessLogWriter>> {
    WRITER.get().and_then(Weak::upgrade)
}

/// Install the subscriber and start the store writer. Call once, after
/// the data root is claimed: a server refused the root must not write
/// into the other server's store. Keep the writer for the life of the
/// process and drop it last — the drop is the final flush. `None` when
/// the store could not be opened, in which case stderr still gets
/// every line.
pub fn init(root: &Path) -> Option<Arc<ProcessLogWriter>> {
    let commit = datalib_runs::git_hash_and_origin();
    let (retention, log_filter) = boot_config(root);
    let writer = ProcessLogWriter::start(
        root,
        Process::Http,
        commit.as_ref().map(|(hash, _)| hash.clone()),
        retention,
    )
    .map(Arc::new);
    if let Some(w) = &writer {
        let _ = PROCESS_ID.set(w.process_id().to_string());
        let _ = WRITER.set(Arc::downgrade(w));
    }
    let store = writer.as_ref().map(|w| StoreLayer::new(Arc::downgrade(w)));
    // `RUST_LOG` is a person's choice and wins; else the config's level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log_filter));
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(true);
    // `try_init`, not `init`: the tests build the state several times
    // in one process, and a second install is not an error worth dying
    // over — the first subscriber keeps working.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(store)
        .try_init();
    datalib_runs::log_build_commit(commit.as_ref());
    tracing::info!(filter = %log_filter, "log filter");
    writer
}

/// The config's `run_history` and the filter its `log_level` asks
/// for, read once at boot; the defaults when there is no config yet
/// or it does not say. A change to either takes a restart.
fn boot_config(root: &Path) -> (Retention, String) {
    let path = datalib_dag::config::root_config_path(root);
    match datalib_dag::config::load_graded(&path).ok() {
        Some((checked, _)) => (
            checked
                .cfg
                .run_history
                .map(|h| h.retention())
                .unwrap_or_default(),
            checked.cfg.log_filter(),
        ),
        None => (Retention::default(), datalib_runs::default_filter()),
    }
}
