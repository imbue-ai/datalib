//! What the loop tells the host that runs it, for a loop that never
//! exits: when a run opens and closes. A run is the run store's grouping
//! of the work done while some request or job was open. Everything else
//! the loop says, it writes to `system/supervisor.sqlite`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use datalib_runs::RunStart;

use crate::events::{Event, EventSink};

/// How a host hears what [`Runner::run_forever`]
/// (`crate::scheduler::Runner`) does.
pub trait LoopHost: Send + Sync {
    /// A run opens, for `start`'s reason. The environment its steps
    /// get, which carries the run's id and "now"
    /// (`DATALIB_DAG_RUN_ID`, `DATALIB_DAG_NOW`); or why nothing can run,
    /// which fails the requests open.
    fn open(&self, start: RunStart) -> Result<BTreeMap<String, String>, String>;
    /// The run `run_id` is over, and its summary has been sent.
    fn close(&self, run_id: &str);
}

/// A sink a host points at each run's sink in the run store while the run
/// is open, and at nothing between: the run store closes the run when its
/// sink is dropped.
#[derive(Default)]
pub struct OpenRunSink(Mutex<Option<Arc<dyn EventSink>>>);

impl OpenRunSink {
    /// Point at `to`, handing back what it pointed at: dropping that is
    /// what closes the run in the run store.
    pub fn point_at(&self, to: Option<Arc<dyn EventSink>>) -> Option<Arc<dyn EventSink>> {
        std::mem::replace(&mut *self.0.lock().expect("a period sink's lock"), to)
    }
}

impl EventSink for OpenRunSink {
    fn emit(&self, event: &Event) {
        if let Some(sink) = self.0.lock().expect("a period sink's lock").as_ref() {
            sink.emit(event);
        }
    }
}
