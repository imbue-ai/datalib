//! The page zoom the View menu sets: the levels it steps through, and the
//! file it is kept in so a restart comes back at it. The shell holds the
//! level and applies it to every window after each page load, so it
//! does not reset when the window moves from the libraries screen to a
//! library. Nothing here may reference `tauri` (BUILD.bazel's
//! `zoom_test` compiles this file alone).

use std::path::{Path, PathBuf};

/// The levels Zoom In and Zoom Out step through, as in a browser.
pub const LEVELS: [f64; 13] = [
    0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0,
];

/// Actual Size.
pub const ACTUAL: f64 = 1.0;

// Levels closer than this are the same level: a stored one is read back
// from text.
const SAME: f64 = 1e-6;

pub fn zoom_in(level: f64) -> f64 {
    LEVELS
        .iter()
        .copied()
        .find(|&l| l > level + SAME)
        .unwrap_or(LEVELS[LEVELS.len() - 1])
}

pub fn zoom_out(level: f64) -> f64 {
    LEVELS
        .iter()
        .rev()
        .copied()
        .find(|&l| l < level - SAME)
        .unwrap_or(LEVELS[0])
}

pub fn zoom_file(config_dir: &Path) -> PathBuf {
    config_dir.join("zoom")
}

/// The kept level, or Actual Size when there is none or it is not one
/// this build would set.
pub fn load(file: &Path) -> f64 {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|&l| l >= LEVELS[0] - SAME && l <= LEVELS[LEVELS.len() - 1] + SAME)
        .unwrap_or(ACTUAL)
}

pub fn save(file: &Path, level: f64) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, format!("{level}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zooming_steps_through_the_levels_and_stops_at_the_ends() {
        assert_eq!(zoom_in(ACTUAL), 1.1);
        assert_eq!(zoom_out(ACTUAL), 0.9);
        assert_eq!(zoom_in(3.0), 3.0);
        assert_eq!(zoom_out(0.5), 0.5);
    }

    /// A level between two steps (one an older build kept) moves to the
    /// next step, not past it.
    #[test]
    fn a_level_between_steps_moves_to_the_nearest_step_that_way() {
        assert_eq!(zoom_in(1.2), 1.25);
        assert_eq!(zoom_out(1.2), 1.1);
    }

    #[test]
    fn a_kept_level_comes_back_and_anything_else_is_actual_size() {
        let tmp = tempfile::tempdir().unwrap();
        let file = zoom_file(&tmp.path().join("config"));
        assert_eq!(load(&file), ACTUAL);
        save(&file, 1.25).unwrap();
        assert_eq!(load(&file), 1.25);
        std::fs::write(&file, "nonsense").unwrap();
        assert_eq!(load(&file), ACTUAL);
        std::fs::write(&file, "40").unwrap();
        assert_eq!(load(&file), ACTUAL);
    }
}
