//! The lines a step prints while it runs (`step_protocol.md` §"stdout:
//! the event protocol"). Each is one JSON object with an `event` field
//! naming its kind and a `step` field, which [`crate::Emitter`] fills.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

crate::closed_set! {
    /// A `log` line's severity.
    pub enum LogLevel {
        Trace = "trace",
        Debug = "debug",
        Info = "info",
        Warn = "warn",
        Error = "error",
    }
}

/// One line a step prints while it runs, other than `problem`,
/// `covered` and `outcome`, which have types of their own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The current value of one series, never a delta. Name it the way
    /// Prometheus would: a running total ends in `_total`. The runner
    /// reads `queued`, `items`, `documents` and `problems` by name.
    Metric {
        name: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        labels: BTreeMap<String, String>,
        value: i64,
    },
    /// How many units of work there are, for a step that counts one
    /// thing; `None` when it is not known. The runner turns this and
    /// [`Event::ProgressInc`] into the `done_total` and `queued` series.
    ProgressLength {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    /// `delta` more units are done.
    ProgressInc { delta: u64 },
    /// What the step is doing now, in its own words.
    ProgressMessage { msg: String },
    Log {
        level: LogLevel,
        msg: String,
        /// The line's own timestamp (RFC 3339, local offset); the runner
        /// stamps the time it read the line when this is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// What a person can do to fix a failure, such as signing in again,
    /// which the app shows apart from the log.
    Hint { msg: String },
    /// The step sealed part of its output and is still running. `rows` is
    /// how many rows this seal added; leave it out when not counted.
    Checkpoint {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u64>,
    },
    /// What the step claims about itself. `streams_output`: a reader of
    /// the output sees each seal whole, so a consumer may start on each
    /// checkpoint; say it before the first one. `version`: the commit the
    /// program was built from.
    Capabilities {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        streams_output: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

impl Event {
    /// The `event` word of every kind, as the wire spells it.
    pub const NAMES: &'static [&'static str] = &[
        "metric",
        "progress_length",
        "progress_inc",
        "progress_message",
        "log",
        "hint",
        "checkpoint",
        "capabilities",
    ];

    /// A `log` event with only a level and a message.
    pub fn log(level: LogLevel, msg: impl Into<String>) -> Event {
        Event::Log {
            level,
            msg: msg.into(),
            ts: None,
            target: None,
            thread: None,
            fields: None,
        }
    }
}
