//! Free space on the data root's disk, over time, for the status bar and
//! `/metrics`.
//!
//! Unlike the root's size (`usage.rs`), this is sampled on a timer whether
//! or not a run is going: every process on the volume moves it, nothing
//! announces that, and a look is one `statvfs`, not a walk. Each recorded
//! sample is appended to the usage store's `disk_free` table, which seeds
//! the line again after a restart.
//!
//! The loop holds every step while the disk is under the config's
//! `[disk_space]` floor (`datalib_dag::disk_space`); this module only
//! reports it, and says `low` by the same two-line rule so the UI can say
//! why.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app_schema::disk_free::DiskFreeRow;
use datalib_core::repo::DynAppRepo;
use datalib_dag::disk_space::{self, DiskFloor, Space};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::usage::{UsageSample, HISTORY_WINDOW};
use crate::watch::{RootEvent, RootFrame, RootTx, Table};

pub const SAMPLE_EVERY: Duration = Duration::from_secs(10);

/// A move smaller than this is not recorded, stored or announced: free
/// space drifts by a few megabytes a minute on a busy volume, a frame is a
/// refetch in every open window, and the store keeps every row.
const MIN_RECORDED_MOVE: u64 = 10_000_000;

/// How many rows to read back at startup: far more than the window holds
/// at one row per sample.
const SEED_ROWS: usize = 200;

/// The whole answer `GET /api/pipeline/disk` gives.
#[derive(Debug, Clone, Serialize)]
pub struct DiskFree {
    /// What may still be written, or null before the first look or when
    /// the volume cannot be measured.
    pub available_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    /// The config's `[disk_space]` lines, defaults filled in.
    pub pause_below_bytes: u64,
    pub resume_at_bytes: u64,
    /// The steps are held: the disk went under the pause line and has not
    /// yet climbed back to the resume line.
    pub low: bool,
    /// Samples inside the window, oldest first, with the one before it
    /// as the carry-in, like every `history` in `usage.rs`.
    pub history: Vec<UsageSample>,
    pub window_secs: u64,
}

#[derive(Debug, Default)]
struct State {
    last: Option<Space>,
    history: VecDeque<UsageSample>,
    floor: Option<DiskFloor>,
    held: bool,
}

#[derive(Debug, Default)]
pub struct DiskFreeMonitor {
    state: RwLock<State>,
}

/// What one look changed.
#[derive(Debug, Default, PartialEq)]
pub struct Observed {
    /// A reader should fetch again: the value moved enough to record, the
    /// steps were held or let go, or the floor moved.
    pub refetch: bool,
    /// The sample to append to the store, when the value moved enough.
    pub record: Option<DiskFreeRow>,
}

impl DiskFreeMonitor {
    pub async fn observe(&self, space: Space, floor: DiskFloor, now_iso: &str) -> Observed {
        let now = datalib_time::split_stamp(now_iso);
        let mut st = self.state.write().await;
        let held = floor.holds(st.held, space.available);
        let crossed = held != st.held;
        let floor_moved = st.floor != Some(floor);
        st.last = Some(space);
        st.floor = Some(floor);
        st.held = held;
        let moved = st
            .history
            .back()
            .is_none_or(|last| last.bytes.abs_diff(space.available) >= MIN_RECORDED_MOVE);
        let record = moved.then(|| {
            st.history.push_back(UsageSample {
                at: now.utc.clone(),
                bytes: space.available,
            });
            crate::usage::prune(&mut st.history);
            DiskFreeRow {
                measured_at_utc: now.utc.clone(),
                tz_offset: now.tz_offset.clone(),
                available_bytes: space.available as i64,
                total_bytes: space.total as i64,
            }
        });
        Observed {
            refetch: moved || crossed || floor_moved,
            record,
        }
    }

    /// The window from the store, newest row first as it reads back, and
    /// the newest row as the value to show until the first look lands.
    pub async fn seed(&self, rows: Vec<DiskFreeRow>) {
        let Some(newest) = rows.first() else { return };
        let last = Space {
            available: newest.available_bytes.max(0) as u64,
            total: newest.total_bytes.max(0) as u64,
        };
        let samples = rows
            .into_iter()
            .map(|r| UsageSample {
                at: r.measured_at_utc,
                bytes: r.available_bytes.max(0) as u64,
            })
            .collect();
        let mut st = self.state.write().await;
        st.history = crate::usage::seeded_window(samples);
        st.last = Some(last);
    }

    /// `floor` is read by the caller, fresh: an edit to the config shows
    /// at once rather than at the next sample.
    pub async fn snapshot(&self, floor: DiskFloor) -> DiskFree {
        let st = self.state.read().await;
        DiskFree {
            available_bytes: st.last.map(|s| s.available),
            total_bytes: st.last.map(|s| s.total),
            pause_below_bytes: floor.pause_below,
            resume_at_bytes: floor.resume_at,
            low: st.held,
            history: st.history.iter().cloned().collect(),
            window_secs: HISTORY_WINDOW.as_secs(),
        }
    }
}

/// The config's floor, its defaults when it says nothing of one or cannot
/// be read: a config that does not load is reported where it is edited,
/// not here.
pub fn floor_of(config_path: &Path) -> DiskFloor {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| datalib_dag::config::parse_graded(&text).0.disk_space)
        .unwrap_or_default()
}

pub async fn run(
    monitor: Arc<crate::usage::UsageMonitor>,
    repo: DynAppRepo,
    root: Arc<PathBuf>,
    events: RootTx,
) {
    match repo.recent_disk_free(SEED_ROWS).await {
        Ok(rows) => monitor.free.seed(rows).await,
        Err(e) => tracing::warn!("disk: could not read the recorded free space: {e}"),
    }
    let config_path = datalib_dag::config::root_config_path(&root);
    let mut said_unmeasured = false;
    let mut every = tokio::time::interval(SAMPLE_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let (root, path) = (root.clone(), config_path.clone());
        let looked =
            tokio::task::spawn_blocking(move || (disk_space::space(&root), floor_of(&path))).await;
        let Ok((space, floor)) = looked else { continue };
        let space = match space {
            Ok(s) => {
                said_unmeasured = false;
                s
            }
            Err(e) => {
                if !std::mem::replace(&mut said_unmeasured, true) {
                    tracing::warn!("disk: cannot measure the free space on the data root: {e}");
                }
                continue;
            }
        };
        let now_iso = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let seen = monitor.free.observe(space, floor, &now_iso).await;
        if let Some(row) = &seen.record {
            if let Err(e) = repo.record_disk_free(row).await {
                tracing::warn!("disk: could not record a free-space sample: {e}");
            }
        }
        if seen.refetch {
            // `Err` means nobody is subscribed.
            let event = RootEvent::TableChanged { table: Table::Disk };
            let _ = events.send(RootFrame { event, chain: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn space(available: u64) -> Space {
        Space {
            available,
            total: 100 * GB,
        }
    }

    /// Drift under the recording step is neither stored nor announced,
    /// but the number on screen is always the last look.
    #[tokio::test]
    async fn small_drift_is_not_recorded_but_is_shown() {
        let m = DiskFreeMonitor::default();
        let floor = DiskFloor::default();
        let first = m
            .observe(space(50 * GB), floor, "2026-10-10T10:00:00-07:00")
            .await;
        assert!(first.refetch);
        assert_eq!(first.record.unwrap().available_bytes, 50 * GB as i64);
        let drift = m
            .observe(
                space(50 * GB + 4_000_000),
                floor,
                "2026-10-10T10:00:10-07:00",
            )
            .await;
        assert_eq!(drift, Observed::default());
        let moved = m
            .observe(space(49 * GB), floor, "2026-10-10T10:00:20-07:00")
            .await;
        assert!(moved.refetch && moved.record.is_some());
        let snap = m.snapshot(floor).await;
        assert_eq!(snap.available_bytes, Some(49 * GB));
        assert_eq!(snap.history.len(), 2);
        assert!(!snap.low);
    }

    /// The two lines, as the status bar sees them: held from under the
    /// pause line until the resume line, and each crossing announced
    /// however small the move that made it.
    #[tokio::test]
    async fn held_from_the_pause_line_to_the_resume_line() {
        let m = DiskFreeMonitor::default();
        let floor = DiskFloor::default();
        let at = |s: u64| format!("2026-10-10T10:00:{s:02}-07:00");
        m.observe(space(10 * GB + 1), floor, &at(0)).await;
        assert!(!m.snapshot(floor).await.low);

        let under = m.observe(space(10 * GB - 1), floor, &at(10)).await;
        assert!(under.refetch && under.record.is_none(), "{under:?}");
        assert!(m.snapshot(floor).await.low);

        m.observe(space(12 * GB), floor, &at(20)).await;
        assert!(m.snapshot(floor).await.low, "between the lines, still held");

        m.observe(space(15 * GB), floor, &at(30)).await;
        assert!(!m.snapshot(floor).await.low, "at the resume line");
    }

    /// A restart reads the line back from the store, and shows the newest
    /// value at once rather than "—" until the first look.
    #[tokio::test]
    async fn seeding_restores_the_value_and_the_window() {
        let m = DiskFreeMonitor::default();
        let now = chrono::Utc::now();
        let row = |ago_secs: i64, available: i64| DiskFreeRow {
            measured_at_utc: (now - chrono::Duration::seconds(ago_secs)).to_rfc3339(),
            tz_offset: None,
            available_bytes: available,
            total_bytes: 100,
        };
        // Newest first, as the store reads back.
        m.seed(vec![row(30, 42), row(60, 41), row(7200, 40), row(9000, 39)])
            .await;
        let snap = m.snapshot(DiskFloor::default()).await;
        assert_eq!(snap.available_bytes, Some(42));
        assert_eq!(snap.total_bytes, Some(100));
        let kept: Vec<u64> = snap.history.iter().map(|s| s.bytes).collect();
        assert_eq!(kept, [40, 41, 42], "the window, led by the one before it");
    }

    #[test]
    fn the_floor_is_read_from_the_config_in_human_units() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("config.toml");
        assert_eq!(
            floor_of(&config),
            DiskFloor::default(),
            "no config: the defaults"
        );
        std::fs::write(&config, "[disk_space]\npause_below_bytes = \"5000 MB\"\n").unwrap();
        assert_eq!(
            floor_of(&config),
            DiskFloor {
                pause_below: 5 * GB,
                resume_at: 10 * GB
            }
        );
    }
}
