//! What every statement this process ran came to, by the name it carries
//! (`tag.rs`): how often it ran, the rows it answered and walked, the time
//! it took, and the rows each of its loops visited, by the table or index
//! the loop read. A step reports these as metrics when it ends; a server
//! serves its own at `GET /metrics`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One name's statements, added up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagWork {
    pub runs: u64,
    /// Rows returned, or for a write, the rows it changed if more.
    pub rows: u64,
    /// Rows the statement's loops visited, every loop's added: what it
    /// read to find what it answered.
    pub rows_scanned: u64,
    pub vm_steps: u64,
    pub micros: u64,
    pub max_micros: u64,
    /// The rows each loop visited, by SQLite's account of it:
    /// `SCAN t`, `SEARCH t USING INDEX t_by_x (x=?)`. Which index a
    /// statement used is here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub loops: BTreeMap<String, u64>,
}

/// One run of one statement, as [`TagWork::add`] takes it.
#[derive(Debug, Clone, Copy)]
pub struct OneRun<'a> {
    pub rows: u64,
    pub vm_steps: u64,
    pub nanos: u64,
    pub loops: &'a [(&'a str, u64)],
}

impl OneRun<'_> {
    pub fn rows_scanned(&self) -> u64 {
        self.loops.iter().map(|(_, n)| n).sum()
    }
}

impl TagWork {
    pub fn add(&mut self, run: &OneRun<'_>) {
        let micros = run.nanos / 1_000;
        self.runs += 1;
        self.rows += run.rows;
        self.rows_scanned += run.rows_scanned();
        self.vm_steps += run.vm_steps;
        self.micros += micros;
        self.max_micros = self.max_micros.max(micros);
        for (name, visited) in run.loops {
            match self.loops.get_mut(*name) {
                Some(n) => *n += visited,
                None => {
                    self.loops.insert((*name).to_string(), *visited);
                }
            }
        }
    }

    /// `other`'s work added to this.
    pub fn merge(&mut self, other: &TagWork) {
        self.runs += other.runs;
        self.rows += other.rows;
        self.rows_scanned += other.rows_scanned;
        self.vm_steps += other.vm_steps;
        self.micros += other.micros;
        self.max_micros = self.max_micros.max(other.max_micros);
        for (name, visited) in &other.loops {
            *self.loops.entry(name.clone()).or_default() += visited;
        }
    }

    /// What `self` adds to `before`: the work done between two readings.
    pub fn since(&self, before: &TagWork) -> TagWork {
        TagWork {
            runs: self.runs - before.runs,
            rows: self.rows - before.rows,
            rows_scanned: self.rows_scanned - before.rows_scanned,
            vm_steps: self.vm_steps - before.vm_steps,
            micros: self.micros - before.micros,
            max_micros: self.max_micros,
            loops: self
                .loops
                .iter()
                .map(|(k, v)| (k.clone(), v - before.loops.get(k).copied().unwrap_or(0)))
                .filter(|(_, v)| *v > 0)
                .collect(),
        }
    }
}

/// Work by name.
pub type ByTag = BTreeMap<String, TagWork>;

/// `after`'s work less `before`'s, name by name: what ran between two
/// readings of [`totals`].
pub fn since(after: &ByTag, before: &ByTag) -> ByTag {
    after
        .iter()
        .filter_map(|(name, w)| {
            let d = match before.get(name) {
                Some(b) => w.since(b),
                None => w.clone(),
            };
            (d.runs > 0).then(|| (name.clone(), d))
        })
        .collect()
}

/// `more` added to `into`, name by name.
pub fn merge_into(into: &mut ByTag, more: &ByTag) {
    for (name, w) in more {
        into.entry(name.clone()).or_default().merge(w);
    }
}

/// One metric series a statement's work makes, as a step reports it and
/// `GET /metrics` serves it: a running total, labelled with the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    pub name: &'static str,
    pub labels: Vec<(&'static str, String)>,
    pub value: u64,
}

/// The series `by` makes: per name, how often it ran, the rows it
/// answered and walked, and its time in microseconds; per name and loop,
/// the rows each loop visited.
pub fn series(by: &ByTag) -> Vec<Series> {
    let mut out = Vec::new();
    for (sql, w) in by {
        let label = || vec![("sql", sql.clone())];
        for (name, value) in [
            (datalib_metrics::SQL_RUNS, w.runs),
            (datalib_metrics::SQL_ROWS, w.rows),
            (datalib_metrics::SQL_ROWS_SCANNED, w.rows_scanned),
            (datalib_metrics::SQL_MICROSECONDS, w.micros),
        ] {
            out.push(Series {
                name,
                labels: label(),
                value,
            });
        }
        for (lp, visited) in &w.loops {
            out.push(Series {
                name: datalib_metrics::SQL_LOOP_ROWS_SCANNED,
                labels: vec![("sql", sql.clone()), ("loop", lp.clone())],
                value: *visited,
            });
        }
    }
    out
}

pub(crate) fn add_to(map: &mut HashMap<String, TagWork>, tag: &str, run: &OneRun<'_>) {
    match map.get_mut(tag) {
        Some(w) => w.add(run),
        None => map.entry(tag.to_string()).or_default().add(run),
    }
}

static TOTALS: Mutex<Option<HashMap<String, TagWork>>> = Mutex::new(None);

pub(crate) fn note(tag: &str, run: &OneRun<'_>) {
    let mut totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
    add_to(totals.get_or_insert_with(HashMap::new), tag, run);
}

/// Everything this process's statements did since [`crate::install`], by
/// the name each carries.
pub fn totals() -> ByTag {
    let totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
    totals
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_add_up_loop_by_loop() {
        let mut w = TagWork::default();
        w.add(&OneRun {
            rows: 1,
            vm_steps: 30,
            nanos: 5_000,
            loops: &[("SEARCH t USING INDEX t_x (x=?)", 4)],
        });
        w.add(&OneRun {
            rows: 2,
            vm_steps: 60,
            nanos: 9_000,
            loops: &[("SEARCH t USING INDEX t_x (x=?)", 6), ("SCAN u", 10)],
        });
        assert_eq!(w.runs, 2);
        assert_eq!(w.rows, 3);
        assert_eq!(w.rows_scanned, 20);
        assert_eq!(w.micros, 14);
        assert_eq!(w.max_micros, 9);
        assert_eq!(w.loops["SEARCH t USING INDEX t_x (x=?)"], 10);
        assert_eq!(w.loops["SCAN u"], 10);
    }

    #[test]
    fn the_work_between_two_readings_is_their_difference() {
        let mut w = TagWork::default();
        let run = OneRun {
            rows: 1,
            vm_steps: 30,
            nanos: 5_000,
            loops: &[("SCAN t", 4)],
        };
        w.add(&run);
        let before = w.clone();
        w.add(&run);
        let d = w.since(&before);
        assert_eq!((d.runs, d.rows, d.rows_scanned, d.micros), (1, 1, 4, 5));
        assert_eq!(d.loops["SCAN t"], 4);
    }
}
