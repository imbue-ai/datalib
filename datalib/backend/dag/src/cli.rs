//! `datalib-pipeline`'s command line, here rather than in the binary so
//! that another binary's help can quote a `datalib-pipeline` command and
//! test that it parses.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use datalib_cli_help::Example;

const LONG_ABOUT: &str = "\
Run a Datalib data root's pipeline: the steps its config.toml declares, \
each after the steps it reads.

Every verb takes the data root, or its config.toml.

A sync is a request in <data_root>/system/supervisor.sqlite, tagged --by. \
If another process is already running the loop on this root (the app, a \
`datalib-pipeline serve`, or another sync), this one hands it the request \
and follows it.

A step's id is <group>/<function>: slack/ingest, slack/render_markdown, \
unified_grid_and_search_index/unified_grid_and_search_index.";

pub const EXAMPLES: &[Example] = &[
    Example {
        what: "start a new data root, with a config that has no steps yet",
        command: "datalib-pipeline init ~/datalib",
    },
    Example {
        what: "say what is wrong with a config, all at once, and run nothing",
        command: "datalib-pipeline check ~/datalib",
    },
    Example {
        what: "sync every source",
        command: "datalib-pipeline sync ~/datalib --by claude",
    },
    Example {
        what: "sync one source, and everything that reads it",
        command: "datalib-pipeline sync ~/datalib slack/ingest --by claude",
    },
    Example {
        what: "run one step, here Slack's render, if what it reads moved, then every step that reads it",
        command: "datalib-pipeline sync ~/datalib slack/render_markdown --by claude",
    },
    Example {
        what: "empty a source's documents, so the next sync of its render rebuilds them from what it already downloaded",
        command: "datalib-pipeline reset ~/datalib slack/render_markdown",
    },
    Example {
        what: "empty what a source downloaded, so its next sync downloads it again from nothing",
        command: "datalib-pipeline reset ~/datalib slack/ingest",
    },
    Example {
        what: "free up disk space, losing nothing",
        command: "datalib-pipeline free-disk-space ~/datalib",
    },
    Example {
        what: "free up more, keeping only the latest version of each source's rendered documents",
        command: "datalib-pipeline free-disk-space ~/datalib --drop-old-versions-on-derived-data",
    },
    Example {
        what: "hold the loop on a root until stopped, serving every sync asked for, the way the app does",
        command: "datalib-pipeline serve ~/datalib",
    },
    Example {
        what: "list the open syncs (with their request ids), the steps turned off, the steps running and what each step still owes",
        command: "datalib-pipeline status ~/datalib",
    },
    Example {
        what: "stop one sync, by the request id `status` printed",
        command: "datalib-pipeline stop ~/datalib REQUEST_ID --by claude",
    },
    Example {
        what: "leave a source's embedding out of every sync",
        command: "datalib-pipeline turn-off ~/datalib slack/embed --by claude",
    },
    Example {
        what: "put it back",
        command: "datalib-pipeline turn-on ~/datalib slack/embed",
    },
];

#[derive(Parser)]
#[command(
    name = "datalib-pipeline",
    bin_name = "datalib-pipeline",
    about = "Run a Datalib data root's pipeline: the steps its config.toml declares.",
    long_about = LONG_ABOUT,
    after_help = datalib_cli_help::render(EXAMPLES),
)]
pub struct Cli {
    #[command(subcommand)]
    pub verb: Verb,
}

/// How the steps a verb runs are run.
#[derive(Args, Debug, Clone, Default)]
pub struct StepArgs {
    /// Put this directory first on every step's PATH, so a program
    /// shipped beside the runner resolves by its bare name. Default: the
    /// config's `binary_dir`, else the directory this binary is in.
    #[arg(long, value_name = "DIR")]
    pub binary_dir: Option<PathBuf>,
    /// Run at most this many steps at once.
    #[arg(long, value_name = "N")]
    pub parallelism: Option<NonZeroUsize>,
    /// Run as if it were this instant (RFC 3339, with an offset). For
    /// building fixtures, never for a person's root.
    #[arg(long, value_name = "RFC3339")]
    pub now: Option<String>,
}

#[derive(Subcommand)]
pub enum Verb {
    /// Write a starter config.toml into a data root, creating the root:
    /// no steps yet, and comments saying what a step is. A root that
    /// already has a config is left as it is. Writes nothing else, and
    /// starts nothing.
    Init {
        /// The data root, the directory that will hold config.toml and
        /// everything synced into it.
        data_root: PathBuf,
    },
    /// Report every problem with the config, as `file:line:col`, and run
    /// nothing but each step's `--check`, where its program takes it.
    /// Exits 0 when every entry loads, 1 when the file is not a config at
    /// all, 2 when some entries were dropped or a step would refuse its
    /// params.
    Check {
        /// The data root, or its config.toml.
        root: PathBuf,
    },
    /// Sync these steps and everything downstream of them, and nothing
    /// else; every source when no step is named. A source step (one that
    /// declares no inputs) always runs; any other runs only if it is out
    /// of date. Exits with the request's outcome: 0 done, 2 failed or some
    /// config entry did not load, 130 stopped. Ctrl-C asks this request to
    /// stop; a second Ctrl-C exits at once. stderr is an NDJSON event
    /// stream (run_plan, then step_start, progress_*, log, hint and
    /// step_finish per step, then one run_summary); stdout gets one line
    /// per step at the end.
    Sync {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// The steps to sync, by id; separate them with spaces or commas.
        #[arg(value_name = "STEP_ID", value_delimiter = ',')]
        steps: Vec<String>,
        /// Who is asking. The request records it, and the app shows it
        /// ("started by claude").
        #[arg(long, value_name = "WHO", default_value = "cli")]
        by: String,
        #[command(flatten)]
        run: StepArgs,
        /// Use this run id rather than minting one.
        #[arg(long, value_name = "ID")]
        run_id: Option<String>,
    },
    /// Empty what these steps wrote, keeping its doltlite history, so
    /// their next run starts from nothing; an ingest step's attachment
    /// bytes stay. Then, as the app's Reset does, a step that reads
    /// something is rebuilt at once, and what reads a download runs; a
    /// download is not refilled: sync it to download it again. Done by
    /// whichever loop runs on the root, the app's included.
    Reset {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// The steps to empty, by id; separate them with spaces or commas.
        #[arg(value_name = "STEP_ID", value_delimiter = ',', required = true)]
        steps: Vec<String>,
        #[command(flatten)]
        run: StepArgs,
        /// Use this run id rather than minting one.
        #[arg(long, value_name = "ID")]
        run_id: Option<String>,
    },
    /// Give back the disk every store holds that no commit reaches. Every
    /// commit stays readable, so nothing is lost and nothing runs again.
    /// Done by whichever loop runs on the root, the app's included.
    FreeDiskSpace {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// Also keep only the latest version of the derived data: what
        /// Datalib rebuilds from the downloads, which is each source's
        /// rendered documents. Loses their history; a download's history
        /// is always kept.
        #[arg(long)]
        drop_old_versions_on_derived_data: bool,
        #[command(flatten)]
        run: StepArgs,
        /// Use this run id rather than minting one.
        #[arg(long, value_name = "ID")]
        run_id: Option<String>,
    },
    /// Run the loop on the root until stopped: wait for runner-lock if
    /// another process holds it, ask each step whose program takes
    /// --prepare to prepare, then serve every sync, stop, reset and purge
    /// written to the store, idle or not. SIGINT or SIGTERM stops it: its
    /// steps are interrupted and their requests stay open for the next
    /// loop; a second signal exits at once. stderr is an NDJSON stream of
    /// what the loop has to say that no store holds (holding, sync_started,
    /// sync_over, run_store_written); its log lines go to the run store.
    Serve {
        /// The data root, or its config.toml.
        root: PathBuf,
        #[command(flatten)]
        run: StepArgs,
    },
    /// Print the open sync requests, the steps turned off, the steps
    /// running now, and what each step at rest still owed when its last
    /// run ended, one line each starting `request`, `off`, `running` or
    /// `owed`; a line starting `idle:` when there are none.
    Status {
        /// The data root, or its config.toml.
        root: PathBuf,
    },
    /// Ask one sync request to stop. Its steps stop at their next
    /// consistent point, and what they committed stays.
    Stop {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// The request's id, as `status` prints it.
        request_id: String,
        /// Who is asking.
        #[arg(long, value_name = "WHO", default_value = "cli")]
        by: String,
    },
    /// Leave a step out of every sync until it is turned on, stopping it
    /// if it is running. Whatever reads it waits.
    #[command(name = "turn-off")]
    TurnOff {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// The step's id, `<group>/<function>`.
        step: String,
        /// Who is asking. The app shows "turned off by <WHO>".
        #[arg(long, value_name = "WHO", default_value = "cli")]
        by: String,
    },
    /// Include a step in syncs again. Starts nothing by itself.
    #[command(name = "turn-on")]
    TurnOn {
        /// The data root, or its config.toml.
        root: PathBuf,
        /// The step's id, `<group>/<function>`.
        step: String,
    },
}

/// The config a verb's `root` names: `<dir>/config.toml` for a data root,
/// the path itself for a file. A path that is not there yet is a data
/// root unless it names a `.toml` file, so `check ~/new-lib` points at
/// `init` with the config path it would write.
pub fn config_path(root: &Path) -> PathBuf {
    let names_a_file =
        root.is_file() || (!root.exists() && root.extension().is_some_and(|ext| ext == "toml"));
    if names_a_file {
        root.to_path_buf()
    } else {
        crate::config::root_config_path(root)
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{config_path, Cli, Verb, EXAMPLES};

    #[test]
    fn the_help_examples_parse() {
        Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(Cli::command(), EXAMPLES);
    }

    #[test]
    fn init_takes_a_data_root_not_a_config() {
        let cli = Cli::try_parse_from(["datalib-pipeline", "init", "lib"]).expect("init");
        assert!(matches!(cli.verb, Verb::Init { ref data_root } if data_root.as_os_str() == "lib"));
        assert!(Cli::try_parse_from(["datalib-pipeline", "init"]).is_err());
    }

    /// There is no verb a bare config runs: a sync says so.
    #[test]
    fn a_sync_is_asked_for_by_name() {
        assert!(Cli::try_parse_from(["datalib-pipeline", "lib"]).is_err());
        let cli = Cli::try_parse_from(["datalib-pipeline", "sync", "lib"]).expect("sync");
        assert!(matches!(cli.verb, Verb::Sync { ref steps, .. } if steps.is_empty()));
    }

    #[test]
    fn sync_takes_its_steps_after_the_root_by_spaces_and_commas() {
        let cli = Cli::try_parse_from(["datalib-pipeline", "sync", "lib", "a/x,b/y", "c/z"])
            .expect("parse");
        let Verb::Sync { steps, .. } = cli.verb else {
            panic!("not a sync")
        };
        assert_eq!(steps, ["a/x", "b/y", "c/z"]);
    }

    #[test]
    fn a_reset_names_its_steps_and_a_run_one_step_at_a_time() {
        assert!(Cli::try_parse_from(["datalib-pipeline", "reset", "lib"]).is_err());
        assert!(
            Cli::try_parse_from(["datalib-pipeline", "sync", "lib", "--parallelism", "0"]).is_err()
        );
    }

    #[test]
    fn a_root_is_a_data_root_or_its_config() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let cfg = dir.join("config.toml");
        assert_eq!(config_path(dir), cfg, "an existing directory");
        std::fs::write(&cfg, "").unwrap();
        assert_eq!(config_path(&cfg), cfg, "an existing file");
        let other = dir.join("elsewhere.toml");
        assert_eq!(config_path(&other), other, "a .toml not there yet");
        let new_root = dir.join("new-lib");
        assert_eq!(
            config_path(&new_root),
            new_root.join("config.toml"),
            "a root not there yet"
        );
    }
}
