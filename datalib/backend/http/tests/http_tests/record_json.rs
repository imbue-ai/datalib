//! The loop's rows written as JSON, for tests that set them up: easier to
//! read in a fixture than the struct literal, and written into the store
//! the way the loop would. `requests` are opened (and, when `"closed":
//! true`, closed) through the store as a client and the loop would.

use std::path::Path;

use datalib_dag::supervisor::record::{LastInvocation, LoopState, StepRow};
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_dag::supervisor::tick::StateKind;
use datalib_dag::FailureKind;
use serde_json::Value;

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn texts(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect()
}

fn record(v: &Value) -> LoopState {
    let steps = v
        .get("steps")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, st)| {
            let last_invocation = st.get("last_run").map(|l| LastInvocation {
                invocation_id: text(l, "invocation_id").unwrap_or_default(),
                requests: texts(l, "requests"),
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
                last_invocation,
                last_success_at: text(st, "last_success_at"),
                state: text(st, "state").as_deref().and_then(StateKind::parse),
                requests: texts(st, "requests"),
                ..Default::default()
            };
            (id.clone(), step)
        })
        .collect();
    LoopState { steps }
}

/// Write the rows `json` describes into the loop's store at `root`.
pub async fn write(root: &Path, json: &str) {
    let value: Value = serde_json::from_str(json).unwrap();
    let store = Store::open(root).await.unwrap();
    for r in value
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = text(r, "id").unwrap();
        store
            .open_request_as(Some(&id), &texts(r, "roots"), "ui")
            .await
            .unwrap();
        if r.get("closed").and_then(Value::as_bool) == Some(true) {
            store
                .close_request(&id, RequestOutcome::Done, None)
                .await
                .unwrap();
        }
    }
    store
        .save_state(&Default::default(), &record(&value))
        .await
        .unwrap();
    store.close().await;
}
