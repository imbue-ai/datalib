//! What `datalib-pipeline serve` says to the process that started it,
//! one NDJSON line each on its stderr.

use serde::{Deserialize, Serialize};

/// One thing the loop says to whoever started it, one NDJSON line each
/// on `datalib-pipeline serve`'s stderr. Only what no store holds: everything
/// else a parent reads from `system/supervisor.sqlite` and the run store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServeEvent {
    /// This process holds `runner-lock`: the loop is its from here on.
    Holding,
    /// A run began: the loop is busy until [`ServeEvent::SyncOver`].
    SyncStarted {
        run_id: String,
    },
    SyncOver {
        run_id: String,
    },
    /// This process wrote to the run store, so a reader showing it should
    /// read it again. A filesystem watch would say so too, but late.
    RunStoreWritten,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent reads these off a pipe, so the words are a protocol: each
    /// line names its event in snake case, and reads back as it was sent.
    #[test]
    fn a_serve_event_reads_back_as_it_was_written() {
        let events = [
            ServeEvent::Holding,
            ServeEvent::SyncStarted { run_id: "r".into() },
            ServeEvent::SyncOver { run_id: "r".into() },
            ServeEvent::RunStoreWritten,
        ];
        for event in events {
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<ServeEvent>(&line).unwrap(), event);
        }
        assert_eq!(
            serde_json::to_string(&ServeEvent::RunStoreWritten).unwrap(),
            r#"{"event":"run_store_written"}"#
        );
    }
}
