//! `datalib-applet` — the applet host, one subcommand per applet.
#![allow(clippy::disallowed_macros)]

mod datalib_contacts;
mod gate;
mod slack;
mod unified_document_index;
mod unified_time_series_index;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use datalib_cli_help::Example;

const EXAMPLES: &[Example] = &[
    Example {
        what: "serve the search applet on a port the OS picks, as the gateway does",
        command: "datalib-applet unified_document_index --port 0 --params-file params.json",
    },
    Example {
        what: "search a data root as the app's search box does, and print the answer as JSON",
        command: "datalib-applet search ~/datalib 'from:picard after:2026-10-01' --limit 20",
    },
    Example {
        what: "the same search's next page, by the words of the documents",
        command: "datalib-applet search ~/datalib 'warp core' --tab words --offset 20",
    },
];

#[derive(Parser)]
#[command(
    name = "datalib-applet",
    bin_name = "datalib-applet",
    about = "Applet host for the datalib app, and its search from the command line",
    long_about = "Applet host for the datalib app, and its search from the command line. \
                  `datalib-applet search <data_root> '<query>'` answers as the app's \
                  search box does, as JSON, with no server; `datalib-step help search` \
                  has its query language. The other commands are \
                  applets: datalib-http's gateway starts one for each [[applets]] entry \
                  in the config and reads the port it prints, and you run one yourself \
                  only to work on it (docs/dev/applets.md).",
    after_help = datalib_cli_help::render(EXAMPLES)
)]
struct Cli {
    #[command(subcommand)]
    applet: Which,
    /// Where to write this instance's frontend namespace, before
    /// serving. The last segment is the namespace name, which is the
    /// only channel by which an applet learns which instance it is —
    /// two instances of one command differ solely in configuration.
    #[arg(long, global = true)]
    frontend_dir: Option<PathBuf>,
    /// Port to serve on. Loopback only. `0` — what the gateway passes
    /// — means "any": the OS picks, and [`announce_port`] reports back
    /// which one it picked.
    #[arg(short = 'p', long, global = true)]
    port: Option<u16>,
    /// A JSON file holding the config entry's `params`. A file rather
    /// than an argument because params can carry tokens, and argv is
    /// readable by every user on the machine.
    #[arg(long = "params-file", global = true)]
    params_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Which {
    /// Browse a Slack mirror: channels, then one channel's threads,
    /// then a whole thread.
    Slack,
    /// Serve the grid index and the qmd index: search, columns, the
    /// document list, one document, and the files beside it.
    // The old name stays for a config the server has not rewritten yet.
    #[command(name = "unified_document_index", alias = "unified_index")]
    UnifiedDocumentIndex,
    /// Serve the time-series index: the time series, and their points over
    /// a range, downsampled to a budget.
    #[command(name = "unified_time_series_index")]
    UnifiedTimeSeriesIndex,
    /// The contacts app: create contacts and link handles to them, and
    /// resolve a document's handles for its chips.
    #[command(name = "datalib_contacts")]
    DatalibContacts,
    /// Search a data root the way the app's search box does, with no
    /// server, and print the answer `/search` would give, as JSON.
    Search(SearchArgs),
}

/// `/search`'s parameters as flags, read the same way.
#[derive(clap::Args)]
struct SearchArgs {
    /// The data root: the directory holding `config.toml`.
    data_root: PathBuf,
    /// What you would type in the search box: `field:value` terms and
    /// free text (`datalib-step help search`). `''` lists every row.
    query: String,
    /// Which answer to free text: `fields`, `words` or `meaning`. None is
    /// the search box's own: identifiers from the search terms, the rest
    /// from qmd's hybrid query.
    #[arg(long, value_parser = parse_tab)]
    tab: Option<unified_document_index::tabs::SearchTab>,
    /// Rows in this page.
    #[arg(long)]
    limit: Option<usize>,
    /// Where this page starts: the `next_offset` of the page before.
    #[arg(long)]
    offset: Option<usize>,
    /// Grid columns and directions, `created_at:desc,author`.
    #[arg(long)]
    sort: Option<String>,
    /// One group's rows, as `[[column, value], …]`.
    #[arg(long)]
    within: Option<String>,
}

fn parse_tab(s: &str) -> Result<unified_document_index::tabs::SearchTab, String> {
    use strum::VariantArray;
    s.parse().map_err(|_| {
        let tabs: Vec<&str> = unified_document_index::tabs::SearchTab::VARIANTS
            .iter()
            .map(|t| t.as_str())
            .collect();
        format!("one of {}", tabs.join(", "))
    })
}

pub fn announce_port(port: u16) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    // Best effort on both counts: a gateway that has already given up
    // leaves us writing to a closed pipe, and there is nothing useful
    // to do about it that killing the process would not do worse.
    let _ = writeln!(out, "DATALIB_APPLET_PORT={port}");
    let _ = out.flush();
}

fn main() {
    if let Err(e) = datalib_parent_watch::exit_with_parent(|| {
        datalib_parent_watch::report("datalib-applet: parent gone, exiting");
        std::process::exit(0);
    }) {
        eprintln!("datalib-applet: {e}");
        std::process::exit(2);
    }
    let cli = Cli::parse();
    // JSON on stderr, which the gateway unwraps the way the runner
    // unwraps a step's, so each line is stored at its own level. No
    // OTLP: its exporter needs a runtime this process builds later.
    // A search run by hand says on stderr only what went wrong.
    let quiet = matches!(cli.applet, Which::Search(_)).then(|| "warn".to_string());
    let obs = datalib_obs::ObsArgs {
        log_level: std::env::var("RUST_LOG").ok().or(quiet),
        ..Default::default()
    };
    let _obs = match datalib_obs::init(&obs, "datalib-applet") {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("datalib-applet: logging is not set up: {e:#}");
            None
        }
    };
    // The unified_document_index applet runs qmd; from a release tarball that
    // ships no `runtime/`, the first search fetches the manifest's.
    datalib_fetch::enable_runtime_fetch();
    if let Err(e) = run(cli) {
        tracing::error!(error = %format!("{e:#}"), "the applet stopped");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let params: serde_json::Value = match &cli.params_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read the params file {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?
        }
        None => serde_json::Value::Null,
    };
    // Write, then serve. The order is the contract: the gateway waits
    // for `announce_port` and then scans the store, so announcing
    // early would race the scan.
    match cli.applet {
        Which::Slack => {
            if let Some(dir) = &cli.frontend_dir {
                slack::write_frontend(dir, &params)?;
            }
            let port = cli.port.context("-p <port> is required")?;
            slack::serve(port, &params)
        }
        // Contributes no components, so there is nothing to write
        // first: the app's grid and document views are builtins, and
        // this applet only serves the endpoints behind them.
        Which::UnifiedDocumentIndex => {
            let port = cli.port.context("-p <port> is required")?;
            unified_document_index::serve(port)
        }
        // The app's chart is a builtin; nothing to write first.
        Which::UnifiedTimeSeriesIndex => {
            let port = cli.port.context("-p <port> is required")?;
            unified_time_series_index::serve(port)
        }
        // Builtins draw its chips and popovers; nothing to write first.
        Which::DatalibContacts => {
            let port = cli.port.context("-p <port> is required")?;
            datalib_contacts::serve(port)
        }
        Which::Search(args) => search(args),
    }
}

/// Prints the answer on stdout, and exits 1 with each reason on stderr
/// when the search was not answered as asked, so a script cannot take an
/// empty answer for "nothing matched".
fn search(args: SearchArgs) -> Result<()> {
    use std::io::Write;
    let params = unified_document_index::SearchParams {
        q: Some(args.query),
        limit: args.limit,
        offset: args.offset,
        sort: args.sort,
        through: None,
        within: args.within,
        tab: args.tab,
    };
    let unanswered = match unified_document_index::search_once(args.data_root, params) {
        Ok(answer) => {
            let mut out = std::io::stdout().lock();
            serde_json::to_writer(&mut out, &answer).context("write the answer")?;
            writeln!(out).context("write the answer")?;
            unified_document_index::unanswered(&answer)
        }
        Err(e) => vec![format!("{e:#}")],
    };
    for why in &unanswered {
        eprintln!("datalib-applet search: {why}");
    }
    if !unanswered.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use clap::CommandFactory;

    #[test]
    fn the_help_examples_parse() {
        super::Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(super::Cli::command(), super::EXAMPLES);
    }
}
