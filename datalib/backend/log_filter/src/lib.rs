//! The log filter every datalib process starts from: the level
//! `config.toml` asks for (`log_level`, `trace` when it does not say)
//! for our own lines, unless `RUST_LOG` says otherwise.

/// What our own lines are kept down to when the config does not say.
/// `trace` while the pipeline is being debugged: the run store is
/// cheap and rotates, the card opens at `min_level:info` anyway, and a
/// question about a sync is easier to answer with more than with less.
pub const DEFAULT_LEVEL: &str = "trace";

/// The target of every line about a request that left the machine, from
/// whichever process made it; `target:http.outbound` in the log card lists
/// them. `http.request` is the opposite: a page asking this server.
pub const OUTBOUND_TARGET: &str = "http.outbound";

/// Every first-party crate is `datalib_*`, plus the two standalone
/// binaries whose crate is named for the tool, plus the hand-named targets
/// (`http.request`, `http.loop`, `http.outbound`), which no crate's module
/// path can collide with. A directive matches a target by prefix.
const OUR_TARGETS: [&str; 4] = ["datalib", "file_inventory", "dirtree_diff", "http."];

/// The libraries that would talk over us, kept to warnings whatever
/// the level. `sqlx` matters most: at `debug` it logs every statement,
/// including the writes that put these lines in the store. `axum`
/// names every connection accepted and closed at `trace`, which the
/// request line already says.
const LIBRARY_CAPS: &str =
    "sqlx=warn,hyper=warn,h2=warn,rustls=warn,notify=warn,html5ever=error,axum=info";

/// The `RUST_LOG`-grammar filter for one level of our own lines.
/// Third-party crates follow the level down to `info` and no further:
/// their `debug` is glob compilation, parser state and byte counts,
/// not what a question about a sync needs. The step and the runner
/// that stores its lines share one filter on purpose: a step that
/// filtered at `info` would never hand the runner a `debug` line to
/// keep.
pub fn filter_at(level: &str) -> String {
    let libraries = match level {
        "trace" | "debug" => "info",
        other => other,
    };
    let ours = OUR_TARGETS
        .iter()
        .map(|t| format!("{t}={level}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{libraries},{ours},{LIBRARY_CAPS}")
}

/// The directives in `directives` that can match a target outside our
/// own crates, rejoined; `None` when one names a span or a field, whose
/// reach this does not try to read. A directive reaches a target that
/// starts with its own, so one whose target starts with ours reaches
/// only ours. The process uses this to stop the `log` crate's calls
/// before they reach the filter: none of our crates logs through `log`,
/// so its level can be the libraries' alone.
pub fn library_directives(directives: &str) -> Option<String> {
    if directives.contains('[') {
        return None;
    }
    let theirs = directives
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter(|d| {
            let target = d.split_once('=').map_or(*d, |(t, _)| t);
            !OUR_TARGETS.iter().any(|ours| target.starts_with(ours))
        })
        .collect::<Vec<_>>();
    Some(theirs.join(","))
}

/// [`filter_at`] the default level.
pub fn default_filter() -> String {
    filter_at(DEFAULT_LEVEL)
}

#[cfg(test)]
mod tests {
    use super::{default_filter, filter_at, library_directives};

    #[test]
    fn our_crates_take_the_level_and_libraries_stop_at_info() {
        assert_eq!(
            filter_at("trace"),
            "info,datalib=trace,file_inventory=trace,dirtree_diff=trace,http.=trace,\
             sqlx=warn,hyper=warn,h2=warn,rustls=warn,notify=warn,html5ever=error,axum=info"
        );
        assert!(filter_at("debug").starts_with("info,datalib=debug,"));
        assert!(filter_at("warn").starts_with("warn,datalib=warn,"));
        assert_eq!(default_filter(), filter_at("trace"));
    }

    #[test]
    fn library_directives_drop_only_what_reaches_only_ours() {
        assert_eq!(
            library_directives(&filter_at("trace")).as_deref(),
            Some("info,sqlx=warn,hyper=warn,h2=warn,rustls=warn,notify=warn,html5ever=error,axum=info")
        );
        assert_eq!(
            library_directives("datalib_step=debug,http.loop").as_deref(),
            Some("")
        );
        assert_eq!(
            library_directives("trace, data=debug ").as_deref(),
            Some("trace,data=debug")
        );
        assert_eq!(library_directives("info,html5ever[span]=trace"), None);
    }
}
