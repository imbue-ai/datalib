//! The config's `[disk_space]` table: the free space the data root's disk
//! must keep. The loop starts no step while it does not
//! (`datalib_dag::disk_space`); the server shows the same line.

use datalib_source_common::byte_size::{self, human_bytes};
use serde::Deserialize;

const GB: u64 = 1_000_000_000;

/// The config's `[disk_space]`: under `pause_below` free bytes the loop
/// starts no step and stops every running one, and once paused the steps
/// stay paused until `resume_at`. The gap keeps
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
    /// Whether the steps are paused after a look that found `available`,
    /// given whether they were paused before it.
    pub fn holds(&self, was_held: bool, available: u64) -> bool {
        available
            < if was_held {
                self.resume_at
            } else {
                self.pause_below
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gap between the two lines is the whole point: a disk that
    /// went under the pause and rose back a little stays paused, and a
    /// disk that never went under is not paused anywhere above the pause.
    #[test]
    fn paused_steps_wait_for_the_resume_line_and_others_for_the_pause() {
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
}
