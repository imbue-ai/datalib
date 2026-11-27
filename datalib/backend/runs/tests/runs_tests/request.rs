//! One request's invocations, written as the loop's writer writes them:
//! each step's invocation is `<request>/<step>`, filed under the request
//! the first time anything is written about the step.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use datalib_runs::{
    is_terminal, InvocationMetricRow, InvocationRow, LogRow, LoopWriter, ProcessRow, Retention,
};

pub struct Request {
    pub w: LoopWriter,
    pub id: String,
    started: String,
    filed: Mutex<BTreeSet<String>>,
}

/// Now, as the store stamps it.
pub fn now_utc() -> String {
    datalib_time::IsoOffsetTimestamp::now_local()
        .to_utc_and_offset()
        .0
}

impl Request {
    /// A request whose invocations start now, under default retention.
    pub fn start(root: &Path, id: &str) -> Request {
        Request::dated(root, id, &now_utc(), Retention::default())
    }

    /// A request whose invocations start at `started` (UTC).
    pub fn dated(root: &Path, id: &str, started: &str, retention: Retention) -> Request {
        Request::with(root, id, started, None, retention)
    }

    pub fn with(
        root: &Path,
        id: &str,
        started: &str,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Request {
        Request {
            w: LoopWriter::start(root, git_hash, retention).expect("start the store"),
            id: id.to_string(),
            started: started.to_string(),
            filed: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn invocation_of(&self, step: &str) -> String {
        format!("{}/{step}", self.id)
    }

    /// The step's invocation, running, the first time the step is named.
    fn file(&self, step: &str) {
        if !self.filed.lock().unwrap().insert(step.to_string()) {
            return;
        }
        let id = self.invocation_of(step);
        self.w.invocation(InvocationRow {
            invocation_id: id.clone(),
            step: step.to_string(),
            state: "running".into(),
            attempt: 1,
            started_at_utc: self.started.clone(),
            updated_at_utc: self.started.clone(),
            ..Default::default()
        });
        self.w.served(&id, std::slice::from_ref(&self.id));
    }

    /// The step's invocation as `row` has it; a finished one is finished
    /// when it started.
    pub fn step(&self, row: InvocationRow) {
        self.file(&row.step);
        let finished = is_terminal(&row.state).then(|| self.started.clone());
        self.w.invocation(InvocationRow {
            invocation_id: self.invocation_of(&row.step),
            started_at_utc: self.started.clone(),
            finished_at_utc: row.finished_at_utc.clone().or(finished),
            ..row
        });
    }

    pub fn metric(&self, row: InvocationMetricRow) {
        self.file(&row.step);
        self.w.metric(InvocationMetricRow {
            invocation_id: self.invocation_of(&row.step),
            ..row
        });
    }

    pub fn log(&self, row: LogRow) {
        let invocation_id = row.step.as_deref().map(|step| {
            self.file(step);
            self.invocation_of(step)
        });
        self.w.log(LogRow {
            invocation_id,
            ..row
        });
    }

    pub fn process(&self, row: ProcessRow) {
        let invocation_id = row.step.as_deref().map(|step| {
            self.file(step);
            self.invocation_of(step)
        });
        self.w.process(ProcessRow {
            invocation_id,
            ..row
        });
    }

    pub async fn flush(&self) {
        self.w.flush().await;
    }
}
