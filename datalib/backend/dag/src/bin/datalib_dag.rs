//! `datalib-dag` — run a DAG config file (see `datalib_dag::config`
//! for the schema).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use datalib_cli_help::Example;

// `DATALIB_VERSION` is `git describe` at build time under Bazel
// release stamping (see BUILD.bazel `rustc_env_files`); dev builds and
// cargo builds see the unsubstituted placeholder / nothing, rendered
// as "dev".
const VERSION_RESOLVED: &str = {
    let raw = match option_env!("DATALIB_VERSION") {
        Some(r) => r,
        None => "",
    };
    if raw.is_empty() || raw.as_bytes()[0] == b'{' {
        "dev"
    } else {
        raw
    }
};
use datalib_dag::events::FanOutSink;
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::reload::ConfigFile;
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_dag::{config, subprocess, EventSink, NdjsonSink, Runner};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// After the parent is gone — or the terminal is — how long the steps
/// get to checkpoint on their SIGINT before they are killed: the grace a
/// stopped step gets anywhere. Used where no second signal is coming, so
/// the runner has to escalate on its own.
const PARENT_GONE_GRACE: std::time::Duration = datalib_dag::step::STOP_GRACE;

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

const EXAMPLES: &[Example] = &[
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
    version = VERSION_RESOLVED,
    about = "Sync a datalib data root: run the steps its config.toml declares.",
    long_about = LONG_ABOUT,
    after_help = datalib_cli_help::render(EXAMPLES),
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    verb: Option<Verb>,
    /// The config file, `<data_root>/config.toml`.
    #[arg(required = true)]
    config: Option<PathBuf>,
    /// Report every problem with the config, as `file:line:col`, and run
    /// nothing. Exits 0 when every entry loads, 1 when the file is not a
    /// config at all, 2 when some entries were dropped.
    #[arg(long)]
    check: bool,
    /// Run these steps and everything downstream of them, and nothing
    /// else. A source step (one with no inputs) always runs; any other
    /// runs only if it is out of date. Repeat it, or separate ids with
    /// commas.
    #[arg(long, value_name = "STEP_ID", value_delimiter = ',')]
    sync: Vec<String>,
    /// Empty what these steps wrote, keeping its doltlite history, so
    /// their next run starts from nothing; an ingest step's attachment
    /// bytes stay. Alone, that is all this invocation does; with --sync it
    /// runs first. Refused while anything else runs the loop on the root.
    #[arg(long, value_name = "STEP_ID", value_delimiter = ',')]
    reset: Vec<String>,
    /// Who is asking. The request records it, and the app shows it
    /// ("started by claude").
    #[arg(long, value_name = "WHO", default_value = "cli")]
    by: String,
    /// Put this directory first on every step's PATH, so a bare
    /// `datalib-step` resolves there. Default: the config's `binary_dir`,
    /// else the directory this binary is in.
    #[arg(long, value_name = "DIR")]
    binary_dir: Option<PathBuf>,
    /// Run at most this many steps at once.
    #[arg(long, value_name = "N")]
    parallelism: Option<NonZeroUsize>,
    /// Run as if it were this instant (RFC 3339, with an offset). For
    /// building fixtures, never for a person's root.
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    /// Use this run id rather than minting one.
    #[arg(long, value_name = "ID")]
    run_id: Option<String>,
}

/// The steering verbs: each writes a row and returns, and whoever runs the
/// loop acts on it within a second.
#[derive(Subcommand)]
enum Verb {
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // First, so a spawner that asked for the watch and forgot the pipe
    // is refused before the runner lock is taken. A loop whose spawner is
    // gone has nobody left to report to, and holds a lock the next one
    // is waiting on.
    // Every way this process is told to stop goes through the round's
    // stop, so a stopped round starts nothing new while it winds down.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stop_tx = Arc::new(stop_tx);
    let parent_stop = stop_tx.clone();
    datalib_parent_watch::exit_with_parent(move || {
        datalib_parent_watch::report("datalib-dag: parent gone; stopping the round");
        let _ = parent_stop.send(true);
        std::thread::sleep(PARENT_GONE_GRACE);
        datalib_parent_watch::report("datalib-dag: steps still running after the grace, exiting");
        subprocess::kill_children();
        std::process::exit(130);
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // clap's own exit code for a usage error is 2, which here
            // means a sync failed; a command line that does not parse is
            // a setup error, 1.
            let _ = e.print();
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };
    let Cli {
        verb,
        config: config_path,
        check: check_only,
        sync: sync_only,
        reset,
        by,
        binary_dir,
        parallelism,
        now,
        run_id,
    } = cli;
    if let Some(verb) = verb {
        return run_verb(verb).await;
    }
    let config_path = config_path.expect("clap requires a config when there is no verb");
    let trimmed = |ids: Vec<String>| -> Vec<String> {
        ids.into_iter().map(|s| s.trim().to_string()).collect()
    };
    let sync_only = trimmed(sync_only);
    let reset = trimmed(reset);

    let (checked, data_root) = config::load_graded(&config_path)?;

    // Say what is wrong before doing anything, whether or not we are
    // about to run. Printed to stderr so a `--check` used in a pipeline
    // and a real run both put diagnostics in the same place, and
    // neither mixes them into the per-step report on stdout.
    if !checked.is_clean() {
        #[allow(clippy::disallowed_macros)]
        {
            eprintln!("{}", checked.render(&config_path));
        }
    }
    if checked.is_fatal() {
        // Nothing loaded, so there is nothing to run and nothing more
        // to say. Exit 1: a setup error, the same as an unreadable
        // file, because that is what it is.
        bail!(
            "{} is not a config: nothing in it could be read",
            config_path.display()
        );
    }
    // A root a newer line of datalib wrote is refused here, before the
    // runner lock and before the record is read: every step's open would
    // refuse anyway (`datalib_store_meta::guard`). `--check` reports it
    // the same way.
    let newer = datalib_store_meta::inspect_root(&data_root).await;
    if !newer.is_empty() {
        let lines: Vec<String> = newer.iter().map(ToString::to_string).collect();
        bail!(
            "{} was written by a newer datalib; this build ({}) will not touch it:\n{}",
            data_root.display(),
            datalib_runtime::build_id::DATALIB_VERSION,
            lines.join("\n")
        );
    }
    if check_only {
        #[allow(clippy::disallowed_macros)]
        {
            println!(
                "{}: {} step(s), {} applet(s){}",
                config_path.display(),
                checked.cfg.steps.len(),
                checked.cfg.applets.len(),
                match checked.dropped() {
                    0 => String::new(),
                    n => format!(", {n} entr{} dropped", if n == 1 { "y" } else { "ies" }),
                }
            );
        }
        // Same door as `PUT /api/config`: a warning drops nothing, so it
        // is printed above and does not fail the check.
        std::process::exit(if checked.nothing_dropped() { 0 } else { 2 });
    }
    let dropped_entries = checked.dropped();
    let cfg = checked.cfg;
    let graph = checked.graph;

    if let Some(id) = sync_only.iter().find(|id| !graph.by_id.contains_key(*id)) {
        let mut known: Vec<&str> = graph.steps.iter().map(|s| s.id.as_str()).collect();
        known.sort();
        bail!(
            "--sync {id:?}: no such step. Available: {}",
            known.join(", ")
        );
    }

    // `--reset` empties stores, so it needs the root to itself: it is
    // refused, not queued, while anyone runs the loop.
    let reset_lock = if reset.is_empty() {
        None
    } else {
        Some(acquire_or_explain(&data_root)?)
    };

    // Cancellation. The first SIGINT/SIGTERM asks for *this* request to
    // stop — the loop may be running other people's too — and the loop
    // interrupts its steps (SIGINT on each process group), which stop at
    // their next consistent point (`step_protocol.md` § Signals). A second
    // signal gives up waiting and exits hard, taking the steps with it.
    //
    // SIGHUP is not one of those two. It says the terminal is gone, so
    // there is nobody left to send a second signal and waiting for one
    // would wait forever — the runner escalates on its own timer, the
    // same one it uses when its parent dies. It has to: a step is in a
    // process group of its own, so the kernel's SIGHUP to the
    // foreground group no longer reaches it.
    let (ask_tx, ask_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
        let mut interrupts = 0u32;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
                _ = sighup.recv() => {
                    datalib_parent_watch::report(
                        "datalib-dag: terminal hung up; stopping the round",
                    );
                    let _ = stop_tx.send(true);
                    // Async, so the runtime keeps draining what the
                    // steps say while they stop.
                    tokio::time::sleep(PARENT_GONE_GRACE).await;
                    datalib_parent_watch::report(
                        "datalib-dag: steps still running after the grace, exiting",
                    );
                    subprocess::kill_children();
                    std::process::exit(130);
                }
            }
            interrupts += 1;
            if interrupts >= 2 {
                subprocess::kill_children();
                std::process::exit(130);
            }
            let _ = ask_tx.send(true);
        }
    });

    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("create data_root {}", data_root.display()))?;
    let store = Store::open(&data_root).await?;

    // The request this invocation stands for; none for a reset alone.
    let own = if reset.is_empty() || !sync_only.is_empty() {
        let roots: Vec<String> = if sync_only.is_empty() {
            graph.fringe_ids().into_iter().map(str::to_string).collect()
        } else {
            sync_only.clone()
        };
        let id = store.open_request(&roots, &by).await?;
        // The stop goes through the store, like a stop from anywhere else.
        let (root, id2, by2) = (data_root.clone(), id.clone(), by.clone());
        let mut ask = ask_rx;
        tokio::spawn(async move {
            if ask.wait_for(|asked| *asked).await.is_ok() {
                if let Ok(s) = Store::open(&root).await {
                    let _ = s.request_stop(&id2, &by2).await;
                    s.close().await;
                }
            }
        });
        Some(id)
    } else {
        None
    };

    let _lock = match reset_lock {
        Some(lock) => lock,
        None => {
            let own = own.as_deref().expect("a request unless resetting");
            match follow_or_lock(&data_root, &store, own).await? {
                Taken::Lock(lock) => lock,
                Taken::Closed(outcome) => {
                    store.close().await;
                    std::process::exit(exit_code(outcome, dropped_entries));
                }
            }
        }
    };

    // Whoever held the lock before us may have died holding it.
    let taken = host::take_over(&store, &data_root).await?;
    #[allow(clippy::disallowed_macros)]
    {
        if let Some(dead) = &taken.closed_run {
            eprintln!(
                "datalib-dag: closed run {dead} and {} of its steps, which a loop that died \
                 left open",
                taken.closed_invocations
            );
        }
    }

    let now =
        now.unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
    // With the loop serving several requests, a run is the stretch this
    // process runs it for.
    let run_id = run_id.unwrap_or_else(datalib_dag::scheduler::new_run_id);
    let host::StepEnv {
        vars: child_env,
        log_filter,
    } = host::step_env(&cfg, binary_dir.as_deref(), &[], &now, &run_id)?;

    // stderr stays the stream; the store is the record. Both, not
    // either: the stream is what a terminal or a tee sees as it happens,
    // the store is what a second process reads — live, and after the
    // fact.
    let code = {
        let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::new(NdjsonSink::new(std::io::stderr()))];
        match host::start_record(&data_root, &cfg, &run_id, &now) {
            Some(store) => {
                // The runner's own `tracing` lines go to the store too,
                // as the run's lines with no step — and only there:
                // stderr is the NDJSON event stream, which a fmt layer
                // would interleave prose into.
                let filter = tracing_subscriber::EnvFilter::new(&log_filter);
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(datalib_runs::StoreLayer::new(store.log_sink()))
                    .try_init();
                datalib_runs::log_build_commit(datalib_runs::git_hash_and_origin().as_ref());
                sinks.push(Arc::new(store));
            }
            None => {
                // No tracing subscriber is installed on this path and
                // there are no indicatif bars, so the macro's usual
                // objection does not apply and `tracing::warn!` would go
                // nowhere at all.
                #[allow(clippy::disallowed_macros)]
                {
                    eprintln!("datalib-dag: run store unavailable; nothing recorded this run");
                }
            }
        }
        let mut runner = Runner::new(&data_root)
            .sink(Arc::new(FanOutSink(sinks)))
            .child_env(child_env)
            .stop_on(stop_rx)
            .reload_from(Arc::new(ConfigFile::new(&config_path)));
        if let Some(p) = parallelism {
            runner = runner.parallelism(p.get());
        }
        // The first time this build runs on the root, every step that takes
        // the verb is asked to migrate before anything else runs
        // (docs/dev/plans/upgrade_on_launch.md); the http server does the
        // same when it takes the lock.
        let build = datalib_dag::supervisor::upgrade::this_build();
        if !store.launch_pass_done(&build).await? {
            let asked = datalib_dag::supervisor::upgrade::steps_to_ask(&graph);
            #[allow(clippy::disallowed_macros)]
            for m in runner.migrate(&graph, &asked).await? {
                match &m.answer {
                    Ok(true) => eprintln!(
                        "datalib-dag: {} needs to run again for this build; --sync it",
                        m.step
                    ),
                    Ok(false) => {}
                    Err(error) => eprintln!("datalib-dag: could not migrate {}: {error}", m.step),
                }
            }
            store.record_launch_pass(&build).await?;
        }
        if !reset.is_empty() {
            runner.reset(&graph, &reset).await?;
        }
        let Some(own) = own else {
            store.close().await;
            return Ok(());
        };
        let report = runner.serve(&graph, &store).await?;

        #[allow(clippy::disallowed_macros)]
        for s in &report.steps {
            println!(
                "{:<32} {:?}{}",
                s.id,
                s.status,
                s.error
                    .as_deref()
                    .map(|e| format!("  ({e})"))
                    .unwrap_or_default()
            );
        }
        // A dropped config entry is reported here as well as before
        // the run, because the per-step report is the thing a person
        // actually reads at the end — and an entry that was dropped has
        // no row in it, so silence would read as "everything ran".
        if dropped_entries > 0 {
            #[allow(clippy::disallowed_macros)]
            {
                println!(
                    "\n{dropped_entries} config entr{} dropped and did not run; \
                     see the diagnostics above, or run `datalib-dag --check {}`",
                    if dropped_entries == 1 {
                        "y was"
                    } else {
                        "ies were"
                    },
                    config_path.display()
                );
            }
        }
        // Still open only when the host stopped the loop under it: a
        // request nobody is waiting on must not be left for the next loop.
        let outcome = match store.request(&own).await?.and_then(|r| r.closed) {
            Some(outcome) => outcome,
            None => {
                store
                    .close_request(&own, RequestOutcome::Stopped, None)
                    .await?;
                Some(RequestOutcome::Stopped)
            }
        };
        store.close().await;
        exit_code(outcome, dropped_entries)
    }; // every sink drops here, so the store flushes and joins before we exit
    std::process::exit(code);
}

enum Taken {
    /// This process runs the loop.
    Lock(datalib_dag::lock::RunnerLock),
    /// Another process ran it, and the request is over.
    Closed(Option<RequestOutcome>),
}

/// Take the loop, or follow the request while someone else runs it. The
/// lock is tried again on every announcement, not just once: the loop that
/// was running may end between our look and our request landing, and then
/// nobody is left to serve it but us. Its release is announced.
async fn follow_or_lock(data_root: &Path, store: &Store, own: &str) -> Result<Taken> {
    let mut listener = datalib_dag::supervisor::announce::Listener::new(store, "datalib-dag");
    let mut announced = false;
    loop {
        match datalib_dag::lock::try_acquire_runner(data_root) {
            Ok(lock) => return Ok(Taken::Lock(lock)),
            Err(e) if e.is_held() => {
                if !announced {
                    announced = true;
                    #[allow(clippy::disallowed_macros)]
                    {
                        eprintln!(
                            "datalib-dag: the loop on {} is already running{}; \
                             following request {own} there",
                            data_root.display(),
                            e.holder().map(|h| format!(" ({h})")).unwrap_or_default()
                        );
                    }
                }
                if let Some(closed) = store.request(own).await?.and_then(|r| r.closed) {
                    #[allow(clippy::disallowed_macros)]
                    {
                        println!(
                            "request {own}: {}",
                            closed.map(RequestOutcome::as_str).unwrap_or("closed")
                        );
                    }
                    return Ok(Taken::Closed(closed));
                }
                listener.next().await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn acquire_or_explain(data_root: &Path) -> Result<datalib_dag::lock::RunnerLock> {
    datalib_dag::lock::acquire_runner(data_root).map_err(|e| {
        if e.is_held() {
            anyhow::anyhow!(
                "the loop on {} is running{}, and --reset needs the root to itself. \
                 Wait for it to finish.\n(lock: {})",
                data_root.display(),
                match e.holder() {
                    Some(h) => format!(" — {h}"),
                    None => String::new(),
                },
                e.path().display()
            )
        } else {
            anyhow::anyhow!("{e}")
        }
    })
}

fn exit_code(outcome: Option<RequestOutcome>, dropped_entries: usize) -> i32 {
    match outcome {
        Some(RequestOutcome::Done) if dropped_entries == 0 => 0,
        Some(RequestOutcome::Stopped) => 130,
        _ => 2,
    }
}

impl Verb {
    fn config(&self) -> &Path {
        match self {
            Verb::Status { config, .. }
            | Verb::Stop { config, .. }
            | Verb::TurnOff { config, .. }
            | Verb::TurnOn { config, .. } => config,
        }
    }
}

async fn run_verb(verb: Verb) -> Result<()> {
    let (checked, data_root) = config::load_graded(verb.config())?;
    if checked.is_fatal() {
        bail!(
            "{} is not a config: nothing in it could be read",
            verb.config().display()
        );
    }
    let store = Store::open(&data_root).await?;
    let said = match &verb {
        Verb::Status { .. } => status_lines(&store).await?,
        Verb::Stop {
            request_id: id, by, ..
        } => match store.request(id).await?.map(|r| r.closed) {
            None => bail!("no request {id} in {}", data_root.display()),
            Some(Some(outcome)) => vec![format!(
                "request {id} is already over: {}",
                outcome.map(RequestOutcome::as_str).unwrap_or("closed")
            )],
            Some(None) => {
                store.request_stop(id, by).await?;
                vec![format!("asked request {id} to stop")]
            }
        },
        Verb::TurnOff { step, by, .. } => {
            if !checked.graph.by_id.contains_key(step) {
                let mut ids: Vec<&str> = checked.graph.by_id.keys().map(String::as_str).collect();
                ids.sort();
                bail!(
                    "no step {step:?} in this config; its steps: {}",
                    ids.join(", ")
                );
            }
            match store.turned_off().await?.get(step) {
                Some(who) => vec![format!("{step} is already off, turned off by {who}")],
                None => {
                    store.turn_off(step, by).await?;
                    vec![format!("turned off {step}")]
                }
            }
        }
        Verb::TurnOn { step, .. } => match store.turned_off().await?.get(step) {
            None => vec![format!("{step} was not off")],
            Some(who) => {
                store.turn_on(step).await?;
                vec![format!("turned on {step}, which {who} had turned off")]
            }
        },
    };
    store.close().await;
    #[allow(clippy::disallowed_macros)]
    for line in said {
        println!("{line}");
    }
    Ok(())
}

/// One line per open request and per step turned off, in a shape a script can split
/// on two spaces.
async fn status_lines(store: &Store) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for r in store.open_requests().await? {
        let stopping = r
            .stop_requested_by
            .map(|who| format!("  stopping (asked by {who})"))
            .unwrap_or_default();
        lines.push(format!(
            "request {}  by {}  roots {}{stopping}",
            r.id,
            r.opened_by,
            r.roots.join(",")
        ));
    }
    for (step, who) in store.turned_off().await? {
        lines.push(format!("off {step}  by {who}"));
    }
    for inv in store.running_invocations().await? {
        lines.push(format!(
            "running {}  since {}  in run {}",
            inv.step, inv.started_at_utc, inv.run_id
        ));
    }
    if lines.is_empty() {
        lines.push("nothing open, nothing off, nothing running".to_string());
    }
    Ok(lines)
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
