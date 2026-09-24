//! A loop record written as JSON, for tests that set one up: easier to
//! read in a fixture than the struct literal, and written into the store
//! the way the loop would.

use std::path::Path;

use datalib_dag::supervisor::record::{CurrentRun, LastRun, Record, StepRecord};
use serde_json::Value;

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn record(v: &Value) -> Record {
    let steps = v
        .get("steps")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, st)| {
            let last_run = st.get("last_run").map(|l| LastRun {
                run_id: text(l, "run_id").unwrap_or_default(),
                started_at: text(l, "started_at").unwrap_or_default(),
                finished_at: text(l, "finished_at"),
                status: text(l, "status").unwrap_or_default(),
                attempts: l.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32,
                error: text(l, "error"),
            });
            let step = StepRecord {
                succeeded: st
                    .get("succeeded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                last_run,
                last_success_at: text(st, "last_success_at"),
                ..Default::default()
            };
            (id.clone(), step)
        })
        .collect();
    let current_run = v.get("current_run").map(|r| CurrentRun {
        run_id: text(r, "run_id").unwrap_or_default(),
        started_at: text(r, "started_at").unwrap_or_default(),
        finished_at: text(r, "finished_at"),
        states: r
            .get("states")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(id, st)| Some((id.clone(), st.as_str()?.to_string())))
            .collect(),
    });
    Record { steps, current_run }
}

/// Write the record `json` describes into the store at `root`.
pub async fn write(root: &Path, json: &str) {
    let value: Value = serde_json::from_str(json).unwrap();
    let store = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    store
        .save_record(&Default::default(), &record(&value))
        .await
        .unwrap();
    store.close().await;
}
