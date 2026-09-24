//! The log filter every datalib process starts from: the level
//! `config.toml` asks for (`log_level`, `trace` when it does not say)
//! for our own lines, unless `RUST_LOG` says otherwise.

/// What our own lines are kept down to when the config does not say.
/// `trace` while the pipeline is being debugged: the run store is
/// cheap and rotates, the card opens at `min_level:info` anyway, and a
/// question about a sync is easier to answer with more than with less.
pub const DEFAULT_LEVEL: &str = "trace";

/// Every first-party crate is `datalib_*`, plus the two standalone
/// binaries whose crate is named for the tool, plus the server's
/// hand-named targets (`http.request`, `http.loop`), which no crate's
/// module path can collide with. A directive matches a target by prefix.
const OUR_TARGETS: [&str; 4] = ["datalib", "fsindex", "dirtree_diff", "http."];

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

/// [`filter_at`] the default level.
pub fn default_filter() -> String {
    filter_at(DEFAULT_LEVEL)
}

#[cfg(test)]
mod tests {
    use super::{default_filter, filter_at};

    #[test]
    fn our_crates_take_the_level_and_libraries_stop_at_info() {
        assert_eq!(
            filter_at("trace"),
            "info,datalib=trace,fsindex=trace,dirtree_diff=trace,http.=trace,\
             sqlx=warn,hyper=warn,h2=warn,rustls=warn,notify=warn,html5ever=error,axum=info"
        );
        assert!(filter_at("debug").starts_with("info,datalib=debug,"));
        assert!(filter_at("warn").starts_with("warn,datalib=warn,"));
        assert_eq!(default_filter(), filter_at("trace"));
    }
}
