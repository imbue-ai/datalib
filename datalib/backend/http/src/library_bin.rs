//! A library's own programs live in its `system/bin/`
//! (`datalib_runtime::layout::BIN_DIR_REL`), which every step's and
//! applet's `PATH` names. They used to be looked up in `~/.datalib/bin`,
//! one directory shared by every library on the machine. At boot, a
//! program the config runs that only that old directory has is copied
//! into the library, so a config that ran before still runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use datalib_dag::config::ConfigCheck;
use datalib_dag::step::StepRun;

/// The old shared directory, `~/.datalib/bin`.
fn home_bin_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")?;
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".datalib").join("bin"))
}

/// Every program the config runs by a bare name, which `PATH` resolves.
/// A package's programs and any path with a `/` are not looked up there.
pub fn bare_programs(checked: &ConfigCheck) -> BTreeSet<String> {
    let steps = checked.graph.steps.iter().filter_map(|s| match &s.run {
        StepRun::Subprocess { argv, .. } => argv.first().cloned(),
        StepRun::InProcess(_) => None,
    });
    let applets = checked
        .cfg
        .applets
        .iter()
        .filter(|a| a.package.is_none())
        .filter_map(|a| shlex::split(&a.command)?.into_iter().next());
    steps
        .chain(applets)
        .filter(|p| !p.is_empty() && !p.contains('/') && !p.contains('\\'))
        .collect()
}

/// Which of `programs` to bring into the library: those the old directory
/// has and nothing earlier on `PATH` already provides.
pub fn to_bring(
    programs: &BTreeSet<String>,
    in_old: impl Fn(&str) -> bool,
    reachable: impl Fn(&str) -> bool,
) -> Vec<String> {
    programs
        .iter()
        .filter(|p| in_old(p) && !reachable(p))
        .cloned()
        .collect()
}

/// Copy each program the config runs from `~/.datalib/bin` into the
/// library's `system/bin/`, when only the old directory has it.
pub fn bring_forward(root: &Path, checked: &ConfigCheck, binary_dir: Option<&Path>) {
    let Some(old) = home_bin_dir() else { return };
    bring_forward_from(root, checked, binary_dir, &old);
}

fn bring_forward_from(root: &Path, checked: &ConfigCheck, binary_dir: Option<&Path>, old: &Path) {
    let bin = root.join(datalib_runtime::layout::BIN_DIR_REL);
    let first = datalib_dag::config::resolve_binary_dir(&checked.cfg, binary_dir);
    let programs = bare_programs(checked);
    let bring = to_bring(
        &programs,
        |p| old.join(p).symlink_metadata().is_ok(),
        |p| bin.join(p).exists() || first.as_ref().is_some_and(|d| d.join(p).is_file()),
    );
    for program in bring {
        match copy_program(&old.join(&program), &bin.join(&program)) {
            Ok(()) => tracing::warn!(
                "{program}: copied from {} into this library's {}. Programs are no longer \
                 looked up in {}; put new ones in the library.",
                old.display(),
                bin.display(),
                old.display()
            ),
            Err(e) => tracing::error!(
                "{program}: this library runs it from {}, which is no longer looked in, and it \
                 could not be copied into {}: {e}",
                old.display(),
                bin.display()
            ),
        }
    }
}

/// A symlink is recreated as one, pointing where it did, so a link to a
/// program being developed keeps following it; anything else is copied
/// with its permissions.
fn copy_program(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let meta = from.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        let target = match from.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, to);
        #[cfg(not(unix))]
        return std::fs::copy(target, to).map(|_| ());
    }
    std::fs::copy(from, to).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(text: &str) -> ConfigCheck {
        datalib_dag::config::check_text(text)
    }

    const CONFIG: &str = r#"
[[groups]]
id = "w"

[[steps]]
group = "w"
function = "ingest"
command = "my-fetcher --out ."

[[steps]]
id = "local"
command = "./scripts/x"

[[applets]]
id = "chart"
command = "my-chart --port 0"
"#;

    #[test]
    fn bare_programs_are_the_ones_path_resolves() {
        let got = bare_programs(&checked(CONFIG));
        assert_eq!(got, ["my-chart", "my-fetcher"].map(String::from).into());
    }

    #[test]
    fn only_a_program_nothing_else_provides_is_brought() {
        let programs = ["a", "b", "c"].map(String::from).into();
        let got = to_bring(&programs, |p| p != "c", |p| p == "b");
        assert_eq!(got, vec!["a".to_string()]);
    }

    /// A library that ran a program from `~/.datalib/bin` still finds it
    /// after the upgrade, and a symlink there stays a symlink.
    #[test]
    fn bring_forward_copies_files_and_keeps_symlinks() {
        let td = tempfile::tempdir().unwrap();
        let old = td.path().join("old");
        let root = td.path().join("lib");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(old.join("my-fetcher"), b"#!/bin/sh\n").unwrap();
        let dev = td.path().join("dev-build");
        std::fs::write(&dev, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dev, old.join("my-chart")).unwrap();

        bring_forward_from(
            &root,
            &checked(CONFIG),
            Some(td.path().join("none").as_path()),
            &old,
        );

        let bin = root.join(datalib_runtime::layout::BIN_DIR_REL);
        assert_eq!(
            std::fs::read(bin.join("my-fetcher")).unwrap(),
            b"#!/bin/sh\n"
        );
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(bin.join("my-chart")).unwrap(), dev);
    }
}
