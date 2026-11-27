// The run store, `system/runs/runs.sqlite`: what every invocation of a step
// did — how it went, its log lines and its metrics — and which requests
// it served, plus the app server's own log. One table per file below.
// Every process that took part is a row in `processes` — the runner, each
// attempt of each step, the server — and every log line points at the
// one that wrote it. The DAG runner writes the invocation tables; `log`
// is written by the runner and by `datalib-http`, which also reads all of
// it, as does anyone with `sqlite3`.
//
// `runs`, `step_runs`, `metrics` and `metric_samples` hold what builds
// before invocations recorded, a row per busy period of the loop. This
// build writes none of them and reads them as they are, until retention
// ages them out.
//
// Every stamp is a `<x>_utc` column — UTC with a `+00:00` suffix — and
// each table carries a `tz_offset` column holding the offset the stamp
// was made in (`+02:00`), the pair every store keeps (AGENTS.md,
// "Timestamp convention").

pub mod invocation_metric_samples {
    include!("invocation_metric_samples.rs");
}

pub mod invocation_metrics {
    include!("invocation_metrics.rs");
}

pub mod invocation_requests {
    include!("invocation_requests.rs");
}

pub mod invocations {
    include!("invocations.rs");
}

pub mod log {
    include!("log.rs");
}

pub mod metric_samples {
    include!("metric_samples.rs");
}

pub mod metrics {
    include!("metrics.rs");
}

pub mod process {
    include!("process.rs");
}

pub mod run {
    include!("run.rs");
}

pub mod step_runs {
    include!("step_runs.rs");
}

pub mod store_changes {
    include!("store_changes.rs");
}

pub use invocation_metric_samples::InvocationMetricSampleRow;
pub use invocation_metrics::InvocationMetricRow;
pub use invocation_requests::InvocationRequestRow;
pub use invocations::InvocationRow;
pub use log::{LogLevel, LogRow, Process, Stream};
pub use metric_samples::MetricSampleRow;
pub use metrics::MetricRow;
pub use process::ProcessRow;
pub use run::{RunKind, RunRow};
pub use step_runs::StepRunRow;
pub use store_changes::{StoreChangeRow, StorePart};

/// Every table's `CREATE TABLE`, in creation order.
pub fn ddl() -> Vec<&'static str> {
    [
        process::DDL,
        run::DDL,
        step_runs::DDL,
        log::DDL,
        metrics::DDL,
        metric_samples::DDL,
        store_changes::DDL,
        invocations::DDL,
        invocation_requests::DDL,
        invocation_metrics::DDL,
        invocation_metric_samples::DDL,
    ]
    .iter()
    .flat_map(|d| d.iter().map(|(_, sql)| *sql))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    /// `log.seq` is the store's own sequence: an `INTEGER` primary key,
    /// which SQLite treats as the rowid and assigns on insert. Anything
    /// else here — `BIGINT`, a composite key — would make the store
    /// number its own lines.
    #[test]
    fn log_seq_is_a_rowid_alias() {
        let (_, ddl) = log::DDL[0];
        assert!(ddl.contains("seq INTEGER NOT NULL"), "{ddl}");
        assert!(ddl.contains("PRIMARY KEY (seq)"), "{ddl}");
    }

    #[test]
    fn every_table_has_a_tz_offset_beside_its_stamps() {
        for sql in ddl().into_iter().filter(|sql| sql.contains("_utc")) {
            assert!(sql.contains("tz_offset"), "{sql}");
        }
    }

    /// strum and serde spell these independently; the store writes the
    /// strum word and the UI switches on the serde one.
    #[test]
    fn log_words_agree_between_strum_and_serde() {
        for &l in LogLevel::VARIANTS {
            assert_eq!(
                serde_json::to_string(&l).unwrap(),
                format!("\"{}\"", l.as_str())
            );
            assert_eq!(LogLevel::parse(l.as_str()), Some(l));
        }
        for &s in Stream::VARIANTS {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
            assert_eq!(Stream::parse(s.as_str()), Some(s));
        }
        for &p in Process::VARIANTS {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
            assert_eq!(Process::parse(p.as_str()), Some(p));
        }
        for &p in StorePart::VARIANTS {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
            assert_eq!(StorePart::parse(p.as_str()), Some(p));
        }
        assert_eq!(LogLevel::parse("fatal"), None, "no guessing");
    }
}
