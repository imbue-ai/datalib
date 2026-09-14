// One row per log line, from every step of every run.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// A line's severity. The store's column is text and every write goes
/// through [`LogLevel::as_str`]; a reader that meets a word this build
/// does not know keeps the word rather than guessing a level.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<LogLevel> {
        s.parse().ok()
    }
}

/// Which of a subprocess's two pipes a line arrived on.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<Stream> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "log", primary_key = "seq")]
pub struct LogRow {
    /// Assigned by the store on insert, monotone within the file: the
    /// tail cursor. A writer leaves it 0.
    #[col(sql = "INTEGER")]
    pub seq: i64,
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    /// `None` for a line about the run rather than one step.
    #[col(sql = "VARCHAR(255)")]
    pub step: Option<String>,
    /// Which invocation of the step within the run; 0 when unknown.
    #[col(sql = "INT")]
    pub attempt: i64,
    /// UTC. The line's own clock when it carried one, else when the
    /// runner read it.
    #[col(sql = "VARCHAR(40)")]
    pub ts: String,
    /// The offset `ts` was written in: the step's for a line that
    /// stamped itself, the runner's otherwise.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// A [`Stream`] word for a subprocess's line; `None` for one the
    /// runner wrote.
    #[col(sql = "VARCHAR(8)")]
    pub stream: Option<String>,
    /// A [`LogLevel`] word.
    #[col(sql = "VARCHAR(8)")]
    pub level: String,
    /// The tracing target, when the line was structured tracing output.
    #[col(sql = "VARCHAR(255)")]
    pub target: Option<String>,
    /// The thread that wrote it, when the line said.
    #[col(sql = "VARCHAR(64)")]
    pub thread: Option<String>,
    #[col(sql = "TEXT")]
    pub msg: String,
    /// A JSON object of the structured fields beyond the message, when
    /// there were any.
    #[col(sql = "TEXT")]
    pub fields: Option<String>,
}
