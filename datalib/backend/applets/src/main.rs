//! `datalib-applet` — the applet host, one subcommand per applet.
#![allow(clippy::disallowed_macros)]

mod datalib_contacts;
mod gate;
#[cfg(test)]
mod hangup_tests;
mod photos;
mod slack;
mod unified_grid_and_search_index;
mod unified_time_series_index;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use datalib_cli_help::Example;

const EXAMPLES: &[Example] = &[
    Example {
        what: "serve the search applet on a port the OS picks, as the gateway does",
        command: "DATALIB_DAG_PARAMS=\"$(cat params.json)\" datalib-applet unified_grid_and_search_index --port 0",
    },
    Example {
        what: "search a data root as the app's search box does, and print the answer as JSON",
        command: "datalib-applet search ~/datalib 'from:picard after:2026-10-01' --limit 20",
    },
    Example {
        what: "the same search's next page, by the words of the documents",
        command: "datalib-applet search ~/datalib 'warp core semantic:off' --offset 20",
    },
];

#[derive(Parser)]
#[command(
    name = "datalib-applet",
    bin_name = "datalib-applet",
    version = datalib_runtime::build_id::DATALIB_VERSION,
    about = "Applet host for the Datalib app, and its search from the command line",
    long_about = "Applet host for the Datalib app, and its search from the command line. \
                  `datalib-applet search <data_root> '<query>'` answers as the app's \
                  search box does, as JSON, with no server; `datalib-step help search` \
                  has its query language. The other commands are \
                  applets: datalib-http's gateway starts one for each [[applets]] entry \
                  in the config and reads the port it prints, and you reach each through \
                  it, at /applet/<id>/. `datalib-step help applets` says what each serves; \
                  you run one yourself only to work on it (`datalib-step help \
                  writing-applets`).",
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
}

#[derive(Subcommand)]
enum Which {
    /// Browse a Slack mirror: channels, then one channel's threads,
    /// then a whole thread.
    Slack,
    /// Serve the grid index and the search index: search, columns, the
    /// document list, one document, and the files beside it.
    // The old names stay for a config the server has not rewritten yet.
    #[command(
        name = "unified_grid_and_search_index",
        aliases = ["unified_document_index", "unified_index"]
    )]
    UnifiedGridAndSearchIndex,
    /// Serve the time-series index: the time series, and their points over
    /// a range, downsampled to a budget.
    #[command(name = "unified_time_series_index")]
    UnifiedTimeSeriesIndex,
    /// The contacts app: who a handle is. Link handles to contacts, label
    /// people and senders, and review what an agent proposed.
    #[command(name = "datalib_contacts", after_help = datalib_contacts::routes_help())]
    DatalibContacts,
    /// Photos and videos: a contact sheet, one photo's details, and its
    /// pictures from the source's own previews.
    Photos,
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
    /// Gone: a search has one answer. Taken so a script still naming a tab
    /// hears why, as `refused`, rather than a usage error.
    #[arg(long, hide = true)]
    tab: Option<String>,
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
    datalib_sql_work::install();
    datalib_embedder::look_for_cuda_module(datalib_runtime::cuda_module::find);
    let _obs = match datalib_obs::init(&obs, "datalib-applet") {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("datalib-applet: logging is not set up: {e:#}");
            None
        }
    };
    if let Err(e) = run(cli) {
        tracing::error!(error = %format!("{e:#}"), "the applet stopped");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // The config entry's `params`, in the environment rather than on
    // argv: they can carry tokens, and argv is readable by every user.
    let params: serde_json::Value = match std::env::var(datalib_env::DATALIB_DAG_PARAMS) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", datalib_env::DATALIB_DAG_PARAMS))?,
        _ => serde_json::Value::Null,
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
        Which::UnifiedGridAndSearchIndex => {
            let port = cli.port.context("-p <port> is required")?;
            unified_grid_and_search_index::serve(port)
        }
        // The app's chart is a builtin; nothing to write first.
        Which::UnifiedTimeSeriesIndex => {
            let port = cli.port.context("-p <port> is required")?;
            unified_time_series_index::serve(port)
        }
        // Builtins draw its chips and cards; nothing to write first.
        Which::DatalibContacts => {
            let port = cli.port.context("-p <port> is required")?;
            datalib_contacts::serve(port)
        }
        Which::Photos => {
            if let Some(dir) = &cli.frontend_dir {
                photos::write_frontend(dir)?;
            }
            let port = cli.port.context("-p <port> is required")?;
            photos::serve(port)
        }
        Which::Search(args) => search(args),
    }
}

/// Prints the answer on stdout, and exits 1 with each reason on stderr
/// when the search was not answered as asked, so a script cannot take an
/// empty answer for "nothing matched".
fn search(args: SearchArgs) -> Result<()> {
    use std::io::Write;
    let params = unified_grid_and_search_index::SearchParams {
        q: Some(args.query),
        limit: args.limit,
        offset: args.offset,
        sort: args.sort,
        through: None,
        among: None,
        within: args.within,
        tab: args.tab,
    };
    let unanswered = match unified_grid_and_search_index::search_once(args.data_root, params) {
        Ok(answer) => {
            let mut out = std::io::stdout().lock();
            serde_json::to_writer(&mut out, &answer).context("write the answer")?;
            writeln!(out).context("write the answer")?;
            unified_grid_and_search_index::unanswered(&answer)
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

    #[test]
    fn the_commands_the_docs_show_parse() {
        let commands = datalib_agent_docs::commands_running("datalib-applet");
        assert!(!commands.is_empty(), "no doc runs datalib-applet");
        datalib_cli_help::assert_argvs_parse(super::Cli::command(), &commands);
    }

    /// `help applets` and the overview list the applets from
    /// `datalib_agent_docs::APPLETS`, so one added here has to be added there.
    #[test]
    fn every_applet_is_in_the_list_the_help_prints() {
        let mut commands: Vec<String> = super::Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|name| name != "search" && name != "help")
            .collect();
        let mut listed: Vec<String> = datalib_agent_docs::APPLETS
            .iter()
            .map(|a| a.name.to_string())
            .collect();
        commands.sort();
        listed.sort();
        assert_eq!(commands, listed);
    }

    /// With no server to ask, `--help` lists what the contacts app serves.
    #[test]
    fn the_contacts_apps_help_lists_its_routes_and_who_each_is_for() {
        let mut cli = super::Cli::command();
        let help = cli
            .find_subcommand_mut("datalib_contacts")
            .expect("the applet")
            .render_help()
            .to_string();
        for line in ["POST /proposals", "POST /link", "GET /review/items"] {
            assert!(help.contains(line), "{line} is not in:\n{help}");
        }
        assert!(help.contains("Routes that are `person`"), "{help}");
    }

    #[test]
    fn version_flag_prints_the_workspace_version() {
        datalib_cli_help::assert_version_flag_prints(
            super::Cli::command(),
            datalib_runtime::build_id::DATALIB_VERSION,
        );
    }
}
