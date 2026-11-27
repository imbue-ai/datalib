//! The loop's rows written as JSON, for tests that set them up: easier to
//! read in a fixture than the struct literal, and written into the store
//! the way the loop would.

use std::path::Path;

use datalib_step_protocol::FailureKind;
use datalib_supervisor_store::record::{CurrentRun, LastRun, LoopState, StepRow};
use datalib_supervisor_store::state::StateKind;
use serde_json::Value;

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn record(v: &Value) -> LoopState {
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
                failure: text(l, "failure").as_deref().and_then(FailureKind::parse),
            });
            let step = StepRow {
                succeeded: st
                    .get("succeeded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                last_run,
                last_success_at: text(st, "last_success_at"),
                state: text(st, "state").as_deref().and_then(StateKind::parse),
                requests: st
                    .get("requests")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| Some(r.as_str()?.to_string()))
                    .collect(),
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
    LoopState { steps, current_run }
}

/// Write the rows `json` describes into the loop's store at `root`.
pub async fn write(root: &Path, json: &str) {
    let value: Value = serde_json::from_str(json).unwrap();
    let store = datalib_supervisor_store::store::Store::open(root)
        .await
        .unwrap();
    store
        .save_state(&Default::default(), &record(&value))
        .await
        .unwrap();
    store.close().await;
}
