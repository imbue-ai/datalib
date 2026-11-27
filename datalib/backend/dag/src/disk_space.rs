//! How much room is left on the volume a data root lives on, and whether
//! the config's `[disk_space]` floor (`datalib_config::disk_floor`) holds
//! the steps back. The loop holds
//! every step while it does (`supervisor/tick.rs`, `Wait::DiskSpace`); the
//! app's server samples the same number for the status bar.

use std::path::Path;

use datalib_config::disk_floor::DiskFloor;
use datalib_source_common::byte_size::human_bytes;

/// One look at a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// What an unprivileged process may still write. On APFS this leaves
    /// out purgeable space, so it can read lower than Finder's "available".
    pub available: u64,
    pub total: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

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
