//! What the loop tells the host that runs it, for a loop that never
//! exits: when a busy period opens and closes, which is all a busy period
//! still is (the run store's grouping of the work done while some request
//! or job was open). Everything else it says, it writes to the store.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use datalib_runs::RunStart;

use crate::events::{Event, EventSink};

/// How a host hears what [`Runner::run_forever`]
/// (`crate::scheduler::Runner`) does.
pub trait LoopHost: Send + Sync {
    /// A period opens, for `start`'s reason. The environment its steps
    /// get, which carries the period's run id and "now"
    /// (`DATALIB_DAG_RUN_ID`, `DATALIB_DAG_NOW`); or why nothing can run,
    /// which fails the requests open.
    fn open(&self, start: RunStart) -> Result<BTreeMap<String, String>, String>;
    /// The period `run_id` is over, and its summary has been sent.
    fn close(&self, run_id: &str);
}

/// A sink a host points at each period's record while it is open, and at
/// nothing between: the record closes when its sink is dropped.
#[derive(Default)]
pub struct PeriodSink(Mutex<Option<Arc<dyn EventSink>>>);

impl PeriodSink {
    /// Point at `to`, handing back what it pointed at: dropping that is
    /// what closes a record.
    pub fn point_at(&self, to: Option<Arc<dyn EventSink>>) -> Option<Arc<dyn EventSink>> {
        std::mem::replace(&mut *self.0.lock().expect("a period sink's lock"), to)
    }
}

impl EventSink for PeriodSink {
    fn emit(&self, event: &Event) {
        if let Some(sink) = self.0.lock().expect("a period sink's lock").as_ref() {
            sink.emit(event);
        }
    }
}
