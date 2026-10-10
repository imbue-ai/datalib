//! How much room is left on the volume a data root lives on, and whether
//! the config's `[disk_space]` floor holds the steps back. The loop holds
//! every step while it does (`supervisor/tick.rs`, `Wait::DiskSpace`); the
//! app's server samples the same number for the status bar.

use std::path::Path;

use datalib_source_common::byte_size;
use serde::Deserialize;

const GB: u64 = 1_000_000_000;

/// One look at a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// What an unprivileged process may still write. On APFS this leaves
    /// out purgeable space, so it can read lower than Finder's "available".
    pub available: u64,
    pub total: u64,
}

/// The config's `[disk_space]`: under `pause_below` free bytes every step
/// is held, and once held they stay held until `resume_at`. The gap keeps
/// a disk hovering at the line from stopping and restarting the same
/// steps over and over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawFloor")]
pub struct DiskFloor {
    pub pause_below: u64,
    pub resume_at: u64,
}

impl Default for DiskFloor {
    fn default() -> Self {
        DiskFloor {
            pause_below: 10 * GB,
            resume_at: 15 * GB,
        }
    }
}

/// The table as written. `resume_at_bytes` left out sits as far above
/// the pause as the defaults do.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloor {
    #[serde(default, deserialize_with = "byte_size::deserialize_opt")]
    pause_below_bytes: Option<u64>,
    #[serde(default, deserialize_with = "byte_size::deserialize_opt")]
    resume_at_bytes: Option<u64>,
}

impl TryFrom<RawFloor> for DiskFloor {
    type Error = String;

    fn try_from(raw: RawFloor) -> Result<Self, String> {
        let default = DiskFloor::default();
        let pause_below = raw.pause_below_bytes.unwrap_or(default.pause_below);
        let gap = default.resume_at - default.pause_below;
        let resume_at = raw
            .resume_at_bytes
            .unwrap_or(pause_below.saturating_add(gap));
        if resume_at < pause_below {
            return Err(format!(
                "disk_space.resume_at_bytes ({}) is under pause_below_bytes ({}): steps would \
                 start again before the disk had the room they paused for",
                human_bytes(resume_at),
                human_bytes(pause_below)
            ));
        }
        Ok(DiskFloor {
            pause_below,
            resume_at,
        })
    }
}

impl DiskFloor {
    /// Whether the steps are held after a look that found `available`,
    /// given whether they were held before it.
    pub fn holds(&self, was_held: bool, available: u64) -> bool {
        available
            < if was_held {
                self.resume_at
            } else {
                self.pause_below
            }
    }
}

/// The steps are held for want of space: what the loop logs and a
/// waiting step's row says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LowDisk {
    pub available: u64,
    pub floor: DiskFloor,
}

impl LowDisk {
    pub fn describe(&self) -> String {
        format!(
            "{} free on the data root's disk; steps pause under {} and run again from {}",
            human_bytes(self.available),
            human_bytes(self.floor.pause_below),
            human_bytes(self.floor.resume_at)
        )
    }
}

#[cfg(unix)]
pub fn space(path: &Path) -> std::io::Result<Space> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `st` is a
    // properly sized, writable statvfs.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The field widths differ between macOS and Linux.
    #[allow(clippy::unnecessary_cast)]
    let block = st.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    Ok(Space {
        available: (st.f_bavail as u64).saturating_mul(block),
        total: (st.f_blocks as u64).saturating_mul(block),
    })
}

#[cfg(not(unix))]
pub fn space(_path: &Path) -> std::io::Result<Space> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "free disk space is only measured on unix",
    ))
}

/// Decimal units, the ones `byte_size` reads, so a log line quotes the
/// floor the way it was likely written.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gap between the two lines is the whole point: a disk that
    /// went under the pause and climbed back a little stays held, and a
    /// disk that never went under is not held anywhere above the pause.
    #[test]
    fn held_steps_wait_for_the_resume_line_and_free_ones_for_the_pause() {
        let floor = DiskFloor::default();
        assert!(
            !floor.holds(false, 12 * GB),
            "between the lines, not held yet"
        );
        assert!(floor.holds(false, 9 * GB), "under the pause");
        assert!(floor.holds(true, 12 * GB), "between the lines, still held");
        assert!(!floor.holds(true, 15 * GB), "at the resume line");
        assert!(!floor.holds(false, 10 * GB), "at the pause line is enough");
    }

    #[test]
    fn the_table_reads_human_units_and_fills_in_what_it_leaves_out() {
        let read = |text: &str| toml::from_str::<DiskFloor>(text).map_err(|e| e.to_string());
        assert_eq!(read(""), Ok(DiskFloor::default()));
        assert_eq!(
            read("pause_below_bytes = \"20 GB\""),
            Ok(DiskFloor {
                pause_below: 20 * GB,
                resume_at: 25 * GB
            }),
            "the resume line keeps the default gap above a pause that moved"
        );
        assert_eq!(
            read("pause_below_bytes = \"5000 MB\"\nresume_at_bytes = \"8 GB\""),
            Ok(DiskFloor {
                pause_below: 5 * GB,
                resume_at: 8 * GB
            })
        );
        assert_eq!(
            read("pause_below_bytes = 0"),
            Ok(DiskFloor {
                pause_below: 0,
                resume_at: 5 * GB
            }),
            "a zero pause never holds anything"
        );
        let err = read("pause_below_bytes = \"20 GB\"\nresume_at_bytes = \"15 GB\"").unwrap_err();
        assert!(err.contains("resume_at_bytes (15.0 GB) is under"), "{err}");
        assert!(
            read("pause_below = \"20 GB\"").is_err(),
            "unknown keys refuse"
        );
    }

    #[test]
    fn the_description_quotes_every_number_in_decimal_units() {
        let low = LowDisk {
            available: 3_200_000_000,
            floor: DiskFloor::default(),
        };
        assert_eq!(
            low.describe(),
            "3.2 GB free on the data root's disk; steps pause under 10.0 GB and run again \
             from 15.0 GB"
        );
        assert_eq!(human_bytes(999), "999 B");
    }

    #[cfg(unix)]
    #[test]
    fn a_real_directory_has_room_and_a_size() {
        let td = tempfile::tempdir().unwrap();
        let s = space(td.path()).unwrap();
        assert!(s.total > 0 && s.available <= s.total, "{s:?}");
        assert!(space(&td.path().join("missing")).is_err());
    }
}
