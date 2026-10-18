//! How much memory each running applet holds: one thread reads every
//! applet's process tree every few seconds, keeping the newest reading and
//! the highest since the applet started. `GET /metrics` serves them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use datalib_dag::memory::TreeMemory;

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

type Table = BTreeMap<String, Reading>;

/// One applet's reading taken while the table was unlocked. It applies only
/// to the process it was taken of: an applet restarted in between has a new
/// pid, and a fresh peak.
fn apply(table: &mut Table, id: &str, pid: u32, rss: u64) {
    if let Some(r) = table.get_mut(id).filter(|r| r.pid == pid) {
        r.rss_bytes = rss;
        r.peak_rss_bytes = r.peak_rss_bytes.max(rss);
    }
}

pub struct AppletMemory {
    table: Arc<Mutex<Table>>,
}

impl Default for AppletMemory {
    fn default() -> Self {
        let table = Arc::new(Mutex::new(Table::new()));
        let weak = Arc::downgrade(&table);
        std::thread::Builder::new()
            .name("applet-memory".into())
            .spawn(move || sample_until_dropped(weak))
            .expect("spawn the applet memory thread");
        Self { table }
    }
}

impl AppletMemory {
    pub fn track(&self, id: &str, pid: u32) {
        let fresh = Reading {
            pid,
            rss_bytes: 0,
            peak_rss_bytes: 0,
        };
        self.lock().insert(id.to_string(), fresh);
    }

    /// The applet's last reading, for the line that says it is gone.
    pub fn forget(&self, id: &str) -> Option<Reading> {
        self.lock().remove(id)
    }

    pub fn readings(&self) -> Table {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Holds only a weak reference, so the thread ends within one interval of
/// the last [`AppletMemory`] going away.
fn sample_until_dropped(table: Weak<Mutex<Table>>) {
    let mut memory = TreeMemory::default();
    loop {
        let Some(strong) = table.upgrade() else {
            return;
        };
        let targets: Vec<(String, u32)> = strong
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, r)| (id.clone(), r.pid))
            .collect();
        if !targets.is_empty() {
            let pids: Vec<u32> = targets.iter().map(|(_, pid)| *pid).collect();
            let measured = memory.measure(&pids);
            let mut t = strong.lock().unwrap_or_else(|e| e.into_inner());
            for ((id, pid), rss) in targets.iter().zip(measured) {
                apply(&mut t, id, *pid, rss);
            }
        }
        drop(strong);
        std::thread::sleep(SAMPLE_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[(&str, u32, u64, u64)]) -> Table {
        rows.iter()
            .map(|&(id, pid, rss_bytes, peak_rss_bytes)| {
                let r = Reading {
                    pid,
                    rss_bytes,
                    peak_rss_bytes,
                };
                (id.to_string(), r)
            })
            .collect()
    }

    #[test]
    fn a_reading_moves_the_current_value_and_only_raises_the_peak() {
        let mut t = table(&[("search", 7, 0, 0)]);
        apply(&mut t, "search", 7, 500);
        apply(&mut t, "search", 7, 200);
        assert_eq!(t, table(&[("search", 7, 200, 500)]));
    }

    /// A reading taken of the process an applet ran as before a restart
    /// must not land on the new one, or the new one starts with the old
    /// one's peak.
    #[test]
    fn a_reading_of_a_replaced_process_is_dropped() {
        let mut t = table(&[("search", 8, 0, 0)]);
        apply(&mut t, "search", 7, 500);
        apply(&mut t, "gone", 9, 500);
        assert_eq!(t, table(&[("search", 8, 0, 0)]));
    }
}
