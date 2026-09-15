//! The server's own log. Every `tracing` event goes two ways: to stderr
//! for whoever started the process, and to the run store for the app —
//! the same `log` table the runner fills, so the Manage screen reads
//! both through one endpoint.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use datalib_runs::{Process, ProcessLogWriter, Retention, StoreLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The same default the other datalib binaries take (`datalib_obs`).
const DEFAULT_FILTER: &str = "info,sqlx=warn,hyper=warn";

/// Install the subscriber and start the store writer. Call once, after
/// the data root is claimed: a server refused the root must not write
/// into the other server's store. Keep the writer for the life of the
/// process and drop it last — the drop is the final flush. `None` when
/// the store could not be opened, in which case stderr still gets
/// every line.
pub fn init(root: &Path) -> Option<Arc<ProcessLogWriter>> {
    let writer = ProcessLogWriter::start(root, Process::Http, retention_of(root)).map(Arc::new);
    let store = writer.as_ref().map(|w| StoreLayer::new(Arc::downgrade(w)));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILTER));
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
    writer
}

/// The config's `run_history`, read once at boot; the default when
/// there is no config yet or it does not say.
fn retention_of(root: &Path) -> Retention {
    let path = datalib_dag::config::root_config_path(root);
    datalib_dag::config::load_graded(&path)
        .ok()
        .and_then(|(checked, _)| checked.cfg.run_history)
        .map(|h| h.retention())
        .unwrap_or_default()
}
