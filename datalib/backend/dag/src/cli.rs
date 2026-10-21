//! `datalib-dag`'s command line, here rather than in the binary so that
//! another binary's help can quote a `datalib-dag` command and test that
//! it parses.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use datalib_cli_help::Example;

const LONG_ABOUT: &str = "\
Sync a datalib data root: run the steps its config.toml declares, each \
after the steps it reads.

A sync is a request in <data_root>/system/supervisor.sqlite, tagged --by. \
If another process is already running the loop on this root (the app, or \
another datalib-dag), this one hands it the request and follows it. Either \
way it exits with the request's outcome: 0 done, 2 failed or some config \
entry did not load, 130 stopped. Ctrl-C asks this request to stop; a second \
Ctrl-C exits at once.

stderr is an NDJSON event stream made to be parsed: run_plan, then \
step_start, progress_*, log, hint and step_finish per step, then one \
run_summary. stdout gets one line per step at the end.

A step's id is <group>/<function>: slack/ingest, slack/render_markdown, \
unified_document_index/grid_index.";

pub const EXAMPLES: &[Example] = &[
    Example {
        what: "say what is wrong with a config, all at once, and run nothing",
        command: "datalib-dag --check ~/datalib/config.toml",
    },
    Example {
        what: "sync every source",
        command: "datalib-dag ~/datalib/config.toml --by claude",
    },
    Example {
        what: "sync one source, and everything that reads it",
        command: "datalib-dag ~/datalib/config.toml --sync slack/ingest --by claude",
    },
    Example {
        what: "run one step, here Slack's render, if what it reads moved, then every step that reads it",
        command: "datalib-dag ~/datalib/config.toml --sync slack/render_markdown --by claude",
    },
    Example {
        what: "rebuild a source's documents from what it already downloaded, whether or not anything moved",
        command: "datalib-dag ~/datalib/config.toml --reset slack/render_markdown --sync slack/render_markdown --by claude",
    },
    Example {
        what: "download a source again from nothing",
        command: "datalib-dag ~/datalib/config.toml --reset slack/ingest --sync slack/ingest --by claude",
    },
    Example {
        what: "list the open syncs (with their request ids), the steps turned off and the steps running",
        command: "datalib-dag status ~/datalib/config.toml",
    },
    Example {
        what: "stop one sync, by the request id `status` printed",
        command: "datalib-dag stop ~/datalib/config.toml REQUEST_ID --by claude",
    },
    Example {
        what: "leave a source's embedding out of every sync",
        command: "datalib-dag turn-off ~/datalib/config.toml slack/embed --by claude",
    },
    Example {
        what: "put it back",
        command: "datalib-dag turn-on ~/datalib/config.toml slack/embed",
    },
];

#[derive(Parser)]
#[command(
    name = "datalib-dag",
    bin_name = "datalib-dag",
    about = "Sync a datalib data root: run the steps its config.toml declares.",
    long_about = LONG_ABOUT,
    after_help = datalib_cli_help::render(EXAMPLES),
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub verb: Option<Verb>,
    /// The config file, `<data_root>/config.toml`.
    #[arg(required = true)]
    pub config: Option<PathBuf>,
    /// Report every problem with the config, as `file:line:col`, and run
    /// nothing. Exits 0 when every entry loads, 1 when the file is not a
    /// config at all, 2 when some entries were dropped.
    #[arg(long)]
    pub check: bool,
    /// Run these steps and everything downstream of them, and nothing
    /// else. A source step (one with no inputs) always runs; any other
    /// runs only if it is out of date. Repeat it, or separate ids with
    /// commas.
    #[arg(long, value_name = "STEP_ID", value_delimiter = ',')]
    pub sync: Vec<String>,
    /// Empty what these steps wrote, keeping its doltlite history, so
    /// their next run starts from nothing; an ingest step's attachment
    /// bytes stay. Alone, that is all this invocation does; with --sync it
    /// runs first. Refused while anything else runs the loop on the root.
    #[arg(long, value_name = "STEP_ID", value_delimiter = ',')]
    pub reset: Vec<String>,
    /// Who is asking. The request records it, and the app shows it
    /// ("started by claude").
    #[arg(long, value_name = "WHO", default_value = "cli")]
    pub by: String,
    /// Put this directory first on every step's PATH, so a bare
    /// `datalib-step` resolves there. Default: the config's `binary_dir`,
    /// else the directory this binary is in.
    #[arg(long, value_name = "DIR")]
    pub binary_dir: Option<PathBuf>,
    /// Run at most this many steps at once.
    #[arg(long, value_name = "N")]
    pub parallelism: Option<NonZeroUsize>,
    /// Run as if it were this instant (RFC 3339, with an offset). For
    /// building fixtures, never for a person's root.
    #[arg(long, value_name = "RFC3339")]
    pub now: Option<String>,
    /// Use this run id rather than minting one.
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,
}

/// The steering verbs: each writes a row and returns, and whoever runs the
/// loop acts on it within a second.
#[derive(Subcommand)]
pub enum Verb {
    /// Print the open sync requests, the steps turned off, and the steps
    /// running now.
    Status {
        /// The config file, `<data_root>/config.toml`.
        config: PathBuf,
        // Every verb once took it; a script may still pass it.
        #[arg(long, hide = true)]
        by: Option<String>,
    },
    /// Ask one sync request to stop. Its steps stop at their next
    /// consistent point, and what they committed stays.
    Stop {
        /// The config file, `<data_root>/config.toml`.
        config: PathBuf,
        /// The request's id, as `status` prints it.
        request_id: String,
        /// Who is asking.
        #[arg(long, value_name = "WHO", default_value = "cli")]
        by: String,
    },
    /// Leave a step out of every sync until it is turned on, stopping it
    /// if it is running. Whatever reads it waits.
    #[command(name = "turn-off", alias = "pause")]
    TurnOff {
        /// The config file, `<data_root>/config.toml`.
        config: PathBuf,
        /// The step's id, `<group>/<function>`.
        step: String,
        /// Who is asking. The app shows "turned off by <WHO>".
        #[arg(long, value_name = "WHO", default_value = "cli")]
        by: String,
    },
    /// Include a step in syncs again. Starts nothing by itself.
    #[command(name = "turn-on", alias = "resume")]
    TurnOn {
        /// The config file, `<data_root>/config.toml`.
        config: PathBuf,
        /// The step's id, `<group>/<function>`.
        step: String,
        #[arg(long, hide = true)]
        by: Option<String>,
    },
}

impl Verb {
    pub fn config(&self) -> &Path {
        match self {
            Verb::Status { config, .. }
            | Verb::Stop { config, .. }
            | Verb::TurnOff { config, .. }
            | Verb::TurnOn { config, .. } => config,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Verb, EXAMPLES};

    #[test]
    fn the_help_examples_parse() {
        Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(Cli::command(), EXAMPLES);
    }

    /// A person may still type what `turn-off` and `turn-on` were called
    /// before the switch, and scripts passed `--by` to every verb.
    #[test]
    fn the_old_spellings_still_parse() {
        let cli = Cli::try_parse_from(["datalib-dag", "pause", "c.toml", "a/src", "--by", "x"])
            .expect("pause");
        assert!(matches!(cli.verb, Some(Verb::TurnOff { ref by, .. }) if by == "x"));
        let cli = Cli::try_parse_from(["datalib-dag", "resume", "c.toml", "a/src", "--by", "x"])
            .expect("resume");
        assert!(matches!(cli.verb, Some(Verb::TurnOn { .. })));
        Cli::try_parse_from(["datalib-dag", "status", "c.toml", "--by", "x"]).expect("status --by");
    }

    #[test]
    fn sync_takes_repeats_and_commas() {
        let cli = Cli::try_parse_from([
            "datalib-dag",
            "c.toml",
            "--sync",
            "a/x,b/y",
            "--sync",
            "c/z",
        ])
        .expect("parse");
        assert_eq!(cli.sync, ["a/x", "b/y", "c/z"]);
    }

    #[test]
    fn a_run_needs_a_config_and_at_least_one_step_at_a_time() {
        assert!(Cli::try_parse_from(["datalib-dag", "--sync", "a/x"]).is_err());
        assert!(Cli::try_parse_from(["datalib-dag", "c.toml", "--parallelism", "0"]).is_err());
    }
}
