//! How much memory each running applet holds, and the row that keeps it.
//! One thread reads every applet's process tree every few seconds, keeping
//! the newest reading and the highest since the applet started.
//! `GET /metrics` serves the readings; the applet's row in the run store's
//! `processes` keeps its start, its end and its peak after the server is
//! gone.

use std::collections::BTreeMap;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use app_schema::runs::{Process, ProcessRow};
use datalib_dag::memory::TreeMemory;
use datalib_runs::ProcessLogWriter;

/// The run store's floor between two samples of a step's series, kept here
/// too so an applet's chart and a step's have the same grain.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub pid: u32,
    /// Zero until the first sample lands.
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Tracked {
    reading: Reading,
    row: ProcessRow,
}

type Table = BTreeMap<String, Tracked>;

/// Where an applet's row is written: the server's store writer, asked at
/// each write, since the server can be without one.
pub type Store = Arc<dyn Fn() -> Option<Arc<ProcessLogWriter>> + Send + Sync>;

fn started(id: &str, pid: u32) -> Tracked {
    let (started_at_utc, tz_offset) = datalib_runs::store::now_split();
    Tracked {
        reading: Reading {
            pid,
            rss_bytes: 0,
            peak_rss_bytes: 0,
        },
        row: ProcessRow {
            process_id: datalib_runs::new_process_id(),
            process: Process::Applet.as_str().into(),
            applet: Some(id.to_string()),
            started_at_utc,
            tz_offset,
            ..Default::default()
        },
    }
}

/// One applet's reading taken while the table was unlocked. It applies only
/// to the process it was taken of: an applet restarted in between has a new
/// pid, and a fresh peak. Returns the applet's row when the peak rose, to
/// be written again.
fn apply(table: &mut Table, id: &str, pid: u32, rss: u64) -> Option<ProcessRow> {
    let t = table.get_mut(id).filter(|t| t.reading.pid == pid)?;
    t.reading.rss_bytes = rss;
    if rss <= t.reading.peak_rss_bytes {
        return None;
    }
    t.reading.peak_rss_bytes = rss;
    t.row.peak_rss_bytes = Some(i64::try_from(rss).unwrap_or(i64::MAX));
    Some(t.row.clone())
}

/// The row of an applet that has stopped: when, and how, where the
/// server saw it end.
fn ended(mut row: ProcessRow, finished_at_utc: String, status: Option<ExitStatus>) -> ProcessRow {
    use std::os::unix::process::ExitStatusExt;
    row.finished_at_utc = Some(finished_at_utc);
    row.exit_code = status.and_then(|s| s.code()).map(i64::from);
    row.signal = status.and_then(|s| s.signal()).map(i64::from);
    row
}

pub struct AppletMemory {
    table: Arc<Mutex<Table>>,
    store: Store,
}

impl Default for AppletMemory {
    fn default() -> Self {
        Self::writing_to(Arc::new(crate::logging::writer))
    }
}

impl AppletMemory {
    pub fn writing_to(store: Store) -> Self {
        let table = Arc::new(Mutex::new(Table::new()));
        let weak = Arc::downgrade(&table);
        let thread_store = store.clone();
        std::thread::Builder::new()
            .name("applet-memory".into())
            .spawn(move || sample_until_dropped(weak, thread_store))
            .expect("spawn the applet memory thread");
        Self { table, store }
    }

    pub fn track(&self, id: &str, pid: u32) {
        let fresh = started(id, pid);
        write(&self.store, fresh.row.clone());
        self.lock().insert(id.to_string(), fresh);
    }

    /// Records how the applet ended, and returns its last reading for the
    /// line that says it is gone. `status` is `None` where the server never
    /// learned it.
    pub fn forget(&self, id: &str, status: Option<ExitStatus>) -> Option<Reading> {
        let gone = self.lock().remove(id)?;
        let finished_at_utc = datalib_runs::store::now_split().0;
        write(&self.store, ended(gone.row, finished_at_utc, status));
        Some(gone.reading)
    }

    pub fn readings(&self) -> BTreeMap<String, Reading> {
        self.lock()
            .iter()
            .map(|(id, t)| (id.clone(), t.reading))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn write(store: &Store, row: ProcessRow) {
    if let Some(writer) = store() {
        writer.process(row);
    }
}

/// Holds only a weak reference, so the thread ends within one interval of
/// the last [`AppletMemory`] going away.
fn sample_until_dropped(table: Weak<Mutex<Table>>, store: Store) {
    let mut memory = TreeMemory::default();
    loop {
        let Some(strong) = table.upgrade() else {
            return;
        };
        let targets: Vec<(String, u32)> = strong
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, t)| (id.clone(), t.reading.pid))
            .collect();
        if !targets.is_empty() {
            let pids: Vec<u32> = targets.iter().map(|(_, pid)| *pid).collect();
            let measured = memory.measure(&pids);
            let raised: Vec<ProcessRow> = {
                let mut t = strong.lock().unwrap_or_else(|e| e.into_inner());
                targets
                    .iter()
                    .zip(measured)
                    .filter_map(|((id, pid), rss)| apply(&mut t, id, *pid, rss))
                    .collect()
            };
            for row in raised {
                write(&store, row);
            }
        }
        drop(strong);
        std::thread::sleep(SAMPLE_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(pid: u32, rss_bytes: u64, peak_rss_bytes: u64) -> Tracked {
        let mut t = started("search", pid);
        t.reading = Reading {
            pid,
            rss_bytes,
            peak_rss_bytes,
        };
        t.row.peak_rss_bytes = (peak_rss_bytes > 0).then_some(peak_rss_bytes as i64);
        t
    }

    #[test]
    fn a_reading_moves_the_current_value_and_only_raises_the_peak() {
        let mut t = Table::from([("search".to_string(), tracked(7, 0, 0))]);
        let raised = apply(&mut t, "search", 7, 500).expect("the peak rose");
        assert_eq!(raised.peak_rss_bytes, Some(500));
        assert_eq!(raised.applet.as_deref(), Some("search"));
        assert_eq!(
            apply(&mut t, "search", 7, 200),
            None,
            "a fall writes nothing"
        );
        assert_eq!(t["search"].reading, tracked(7, 200, 500).reading);
        assert_eq!(t["search"].row.peak_rss_bytes, Some(500));
    }

    /// A reading taken of the process an applet ran as before a restart
    /// must not land on the new one, or the new one starts with the old
    /// one's peak.
    #[test]
    fn a_reading_of_a_replaced_process_is_dropped() {
        let mut t = Table::from([("search".to_string(), tracked(8, 0, 0))]);
        assert_eq!(apply(&mut t, "search", 7, 500), None);
        assert_eq!(apply(&mut t, "gone", 9, 500), None);
        assert_eq!(t["search"].reading, tracked(8, 0, 0).reading);
    }

    /// An applet's row reaches the run store with its id and a peak while
    /// it runs, and keeps both once it has ended, with how it ended: what
    /// is left to read after the server is gone.
    #[tokio::test]
    async fn an_applets_row_in_the_store_has_its_peak_and_its_end() {
        let td = tempfile::tempdir().unwrap();
        let writer = Arc::new(
            ProcessLogWriter::start(
                td.path(),
                Process::Http,
                None,
                datalib_runs::Retention::default(),
            )
            .expect("start the store"),
        );
        let for_store = writer.clone();
        let memory = AppletMemory::writing_to(Arc::new(move || Some(for_store.clone())));
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        memory.track("search", child.id());

        let row = || async {
            writer.flush().await;
            datalib_runs::processes(td.path(), None, Some("applet"), 10)
                .await
                .into_iter()
                .next()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let peak = row().await.and_then(|r| r.peak_rss_bytes);
            if peak.is_some_and(|p| p > 0) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no peak reached the store: {:?}",
                row().await
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        memory.forget("search", Some(status));

        let ended = row().await.expect("the applet's row");
        assert_eq!(ended.applet.as_deref(), Some("search"));
        assert_eq!(ended.run_id, None);
        assert!(ended.finished_at_utc.is_some(), "{ended:?}");
        assert_eq!(ended.signal, Some(i64::from(libc::SIGKILL)));
        assert!(ended.peak_rss_bytes.is_some_and(|p| p > 0), "{ended:?}");
    }

    #[test]
    fn an_ended_row_says_how_it_ended_and_keeps_its_peak() {
        use std::os::unix::process::ExitStatusExt;
        let row = tracked(7, 0, 500).row;
        let killed = ended(row.clone(), "t".into(), Some(ExitStatus::from_raw(9)));
        assert_eq!((killed.exit_code, killed.signal), (None, Some(9)));
        assert_eq!(killed.peak_rss_bytes, Some(500));
        let unseen = ended(row, "t".into(), None);
        assert_eq!(unseen.finished_at_utc.as_deref(), Some("t"));
        assert_eq!((unseen.exit_code, unseen.signal), (None, None));
    }
}
