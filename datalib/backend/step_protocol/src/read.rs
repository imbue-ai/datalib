//! Reading one line of a step's stdout as the runner does.

use serde::Deserialize;

use crate::event::Event;
use crate::outcome::Outcome;
use crate::problems::{Covered, Problem};

/// What one line of a step's stdout is.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// An event, with the `step` the line named (which the runner ignores).
    Event {
        step: String,
        event: Event,
    },
    Problem(Problem),
    Covered(Covered),
    Outcome(Outcome),
    /// Not a protocol line: not a JSON object, or one whose `event` names
    /// no kind this crate knows. The runner keeps it as a log line.
    Text,
}

/// A line whose `event` names a kind of line and does not read as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    /// The `event` word the line gave.
    pub event: String,
    pub error: String,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a {:?} line that does not read: {}",
            self.event, self.error
        )
    }
}

impl std::error::Error for Malformed {}

#[derive(Deserialize)]
struct Received {
    step: String,
    #[serde(flatten)]
    event: Event,
}

pub fn read_line(line: &str) -> Result<Line, Malformed> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(Line::Text);
    };
    let Some(name) = value
        .get("event")
        .and_then(|e| e.as_str())
        .map(str::to_string)
    else {
        return Ok(Line::Text);
    };
    let read = match name.as_str() {
        "outcome" => serde_json::from_value(value).map(Line::Outcome),
        "problem" => serde_json::from_value(value).map(Line::Problem),
        "covered" => serde_json::from_value(value).map(Line::Covered),
        n if Event::NAMES.contains(&n) => {
            serde_json::from_value::<Received>(value).map(|r| Line::Event {
                step: r.step,
                event: r.event,
            })
        }
        _ => return Ok(Line::Text),
    };
    read.map_err(|e| Malformed {
        event: name,
        error: e.to_string(),
    })
}
