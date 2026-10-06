//! `datalib-step topo-sort-config <config.toml>`: put a config file in the
//! order data flows (`datalib_dag::config_order`), keeping the text it
//! replaced beside it as `<file>.bak`.

use std::path::{Path, PathBuf};

use datalib_dag::config::write_owner_only;
use datalib_obs::status_line;
use datalib_runtime::atomic;

/// The exit code: 0 when the file is in order (now, or already); 1 when
/// it is not and `check` only asked, or when anything failed.
pub fn run_cli(path: &Path, check: bool) -> i32 {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            status_line!("error: could not read {shown}: {e}");
            return 1;
        }
    };
    let sorted = match datalib_dag::config_order::sort_config(&text) {
        Ok(Some(sorted)) => sorted,
        Ok(None) => {
            status_line!("{shown} is already in data-flow order.");
            return 0;
        }
        Err(e) => {
            status_line!("error: {shown}: {e}");
            return 1;
        }
    };
    if check {
        status_line!(
            "{shown} is not in data-flow order; `datalib-step topo-sort-config {shown}` sorts it."
        );
        return 1;
    }
    let mut bak = path.as_os_str().to_owned();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    match write_owner_only(&bak, text.as_bytes())
        .and_then(|()| atomic::write_owner_only(path, sorted.as_bytes()))
    {
        Ok(()) => {
            status_line!(
                "Sorted {shown} into data-flow order. The previous file is {}.",
                bak.display()
            );
            0
        }
        Err(e) => {
            status_line!("error: could not rewrite {shown}: {e}");
            1
        }
    }
}
