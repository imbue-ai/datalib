//! The runner's writer of a step's `problems.sqlite`, for a step that
//! sends `problem` and `covered` events instead of writing the file
//! itself (`docs/dev/step_protocol.md` §"Problems"). Problems are written
//! in batches as they arrive, so a reader sees them while the step runs,
//! and cleared once the step has ended.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use datalib_step_problems::{Covered, Ended, Found, StepFile};

use crate::events::{Event, EventSink, LogLevel};
use crate::subprocess::{ENV_NOW, ENV_RUN_ID};

/// A lull this long writes what is waiting, so a problem shows while its
/// step goes quiet.
pub(crate) const FLUSH_AFTER: Duration = Duration::from_secs(1);
const BATCH: usize = 200;

pub(crate) struct Relay {
    data_root: PathBuf,
    step_id: String,
    run: String,
    now: datalib_time::StoredStamp,
    file: Option<StepFile>,
    pending: Vec<Found>,
    covered: Vec<Covered>,
    /// Whether the step sent any problem event this run: a step that sent
    /// none writes its own file, or keeps none, and is left alone.
    spoke: bool,
    sink: Arc<dyn EventSink>,
}

/// What one stdout line asks of the relay, if it is one of its events.
pub(crate) enum Line {
    Problem(Found),
    Covered(Covered),
}

impl Line {
    /// `Err` for a line that names one of these events and does not read
    /// as one: said in the log rather than taken as plain output.
    pub(crate) fn read(v: &serde_json::Value) -> Option<Result<Self, String>> {
        let read = match v.get("event").and_then(|e| e.as_str())? {
            "problem" => serde_json::from_value(v.clone()).map(Line::Problem),
            "covered" => serde_json::from_value(v.clone()).map(Line::Covered),
            _ => return None,
        };
        Some(read.map_err(|e| e.to_string()))
    }
}

impl Relay {
    pub(crate) fn new(
        data_root: PathBuf,
        step_id: &str,
        extra_env: &BTreeMap<String, String>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        let now = extra_env
            .get(ENV_NOW)
            .map(|s| datalib_time::split_stamp(s))
            .unwrap_or_else(|| {
                datalib_time::split_stamp(
                    &datalib_time::IsoOffsetTimestamp::now_local().to_string(),
                )
            });
        let run = extra_env
            .get(ENV_RUN_ID)
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Relay {
            data_root,
            step_id: step_id.to_string(),
            run,
            now,
            file: None,
            pending: Vec::new(),
            covered: Vec::new(),
            spoke: false,
            sink,
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) async fn take(&mut self, line: Line) {
        self.spoke = true;
        match line {
            Line::Problem(found) => {
                self.pending.push(found);
                if self.pending.len() >= BATCH {
                    self.flush().await;
                }
            }
            Line::Covered(c) => self.covered.push(c),
        }
    }

    /// Write what is waiting. A file that will not take it is said in the
    /// step's log; the step goes on.
    pub(crate) async fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending);
        let n = batch.len();
        if let Err(e) = self.write(batch).await {
            self.say(format!(
                "could not record {n} problem(s) in {}: {e:#}",
                datalib_step_problems::file_of(&self.data_root, &self.step_id).display()
            ));
        }
    }

    async fn write(&mut self, batch: Vec<Found>) -> anyhow::Result<()> {
        if self.file.is_none() {
            self.file = Some(
                StepFile::open(&self.data_root, &self.step_id, &self.run, self.now.clone()).await?,
            );
        }
        self.file
            .as_ref()
            .expect("opened above")
            .record(batch)
            .await
    }

    /// End the run: write what is waiting, then clear what it covered and
    /// did not find again. `None` when it is not known how the step
    /// ended, which clears nothing.
    pub(crate) async fn finish(mut self, ended: Option<Ended>) {
        if !self.spoke {
            return;
        }
        self.flush().await;
        if self.file.is_none() {
            if let Err(e) = self.write(Vec::new()).await {
                self.say(format!("could not open the problems file: {e:#}"));
                return;
            }
        }
        let file = self.file.take().expect("opened above");
        let covered = match ended {
            Some(_) => std::mem::take(&mut self.covered),
            None => Vec::new(),
        };
        if let Err(e) = file.finish(&covered, ended.unwrap_or(Ended::Short)).await {
            self.say(format!("could not clear the problems file: {e:#}"));
        }
    }

    fn say(&self, msg: String) {
        self.sink.emit(&Event::Log {
            step: self.step_id.clone(),
            level: LogLevel::Error,
            msg,
            ts: None,
            stream: None,
            target: Some("datalib_dag::problems_relay".into()),
            thread: None,
            fields: None,
        });
    }
}
