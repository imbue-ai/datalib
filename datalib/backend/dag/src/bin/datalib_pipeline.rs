//! `datalib-pipeline` — run a DAG config file (see `datalib_dag::config`
//! for the schema).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, FromArgMatches};

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
use datalib_dag::cli::{Cli, Verb};
use datalib_dag::disk_space::human_bytes;
use datalib_dag::events::FanOutSink;
use datalib_dag::scheduler::PrepareError;
use datalib_dag::subprocess::Shrink;
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::reload::ConfigFile;
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_dag::{config, subprocess, EventSink, Graph, NdjsonSink, Runner};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// After the parent is gone — or the terminal is — how long the steps
/// get to checkpoint on their SIGINT before they are killed: the grace a
/// stopped step gets anywhere. Used where no second signal is coming, so
/// the runner has to escalate on its own.
const PARENT_GONE_GRACE: std::time::Duration = datalib_dag::step::STOP_GRACE;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    datalib_sql_work::install();
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
        datalib_parent_watch::report("datalib-pipeline: parent gone; stopping the round");
        let _ = parent_stop.send(true);
        std::thread::sleep(PARENT_GONE_GRACE);
        datalib_parent_watch::report(
            "datalib-pipeline: steps still running after the grace, exiting",
        );
        subprocess::kill_children();
        std::process::exit(130);
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let parsed = Cli::command()
        .version(VERSION_RESOLVED)
        .try_get_matches()
        .and_then(|m| Cli::from_arg_matches(&m));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) => {
            // clap's own exit code for a usage error is 2, which here
            // means a sync failed; a command line that does not parse is
            // a setup error, 1.
            let _ = e.print();
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };
    let check_only = matches!(cli.verb, Verb::Check { .. });
    let (config_path, sync_only, reset, shrink, by, run, run_id) = match cli.verb {
        Verb::Serve { root, run } => {
            return serve(
                &datalib_dag::cli::config_path(&root),
                run,
                &stop_tx,
                stop_rx,
            )
            .await;
        }
        Verb::Check { root } => (
            root,
            vec![],
            vec![],
            None,
            String::new(),
            Default::default(),
            None,
        ),
        Verb::Sync {
            root,
            steps,
            by,
            run,
            run_id,
        } => (root, steps, vec![], None, by, run, run_id),
        Verb::Reset {
            root,
            steps,
            run,
            run_id,
        } => (root, vec![], steps, None, String::new(), run, run_id),
        Verb::FreeDiskSpace {
            root,
            drop_old_versions_on_derived_data,
            run,
            run_id,
        } => {
            let how = if drop_old_versions_on_derived_data {
                Shrink::Compact
            } else {
                Shrink::Collect
            };
            (root, vec![], vec![], Some(how), String::new(), run, run_id)
        }
        verb => return run_verb(verb).await,
    };
    let datalib_dag::cli::StepArgs {
        binary_dir,
        parallelism,
        now,
    } = run;
    let config_path = datalib_dag::cli::config_path(&config_path);
    let trimmed = |ids: Vec<String>| -> Vec<String> {
        ids.into_iter().map(|s| s.trim().to_string()).collect()
    };
    let sync_only = trimmed(sync_only);
    let reset = trimmed(reset);

    let (checked, data_root) = load_config(&config_path)?;

    // Say what is wrong before doing anything, whether or not we are
    // about to run. Printed to stderr so a `check` used in a pipeline
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
                },
            );
            for line in config::expansion_lines(&checked.expansions) {
                println!("{line}");
            }
        }
        // Same door as `PUT /api/config`: a warning drops nothing, so it
        // is printed above and does not fail the check.
        std::process::exit(if checked.nothing_dropped() { 0 } else { 2 });
    }
    let dropped_entries = checked.dropped();
    let cfg = checked.cfg;
    let graph = checked.graph;

    if let Some(id) = sync_only
        .iter()
        .chain(&reset)
        .find(|id| !graph.by_id.contains_key(*id))
    {
        let mut known: Vec<&str> = graph.steps.iter().map(|s| s.id.as_str()).collect();
        known.sort();
        bail!(
            "no step {id:?} in this config; its steps: {}",
            known.join(", ")
        );
    }

    // `reset` empties stores and `free-disk-space` rewrites them, so
    // each needs the root to itself: refused, not queued, while anyone
    // runs the loop.
    let alone = !reset.is_empty() || shrink.is_some();
    let reset_lock = if alone {
        Some(acquire_or_explain(
            &data_root,
            if shrink.is_some() {
                "free-disk-space"
            } else {
                "reset"
            },
        )?)
    } else {
        None
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
                        "datalib-pipeline: terminal hung up; stopping the round",
                    );
                    let _ = stop_tx.send(true);
                    // Async, so the runtime keeps draining what the
                    // steps say while they stop.
                    tokio::time::sleep(PARENT_GONE_GRACE).await;
                    datalib_parent_watch::report(
                        "datalib-pipeline: steps still running after the grace, exiting",
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

    // The request this invocation stands for; none for a reset or a
    // compaction alone.
    let own = if !alone {
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
            let own = own
                .as_deref()
                .expect("a request unless resetting or compacting");
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
                "datalib-pipeline: closed run {dead} and {} of its steps, which a loop that died \
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
    } = host::step_env(
        &cfg,
        binary_dir.as_deref(),
        &[data_root.join(datalib_runtime::layout::BIN_DIR_REL)],
        &now,
        &run_id,
    )?;

    // stderr stays the stream; the store is the record. Both, not
    // either: the stream is what a terminal or a tee sees as it happens,
    // the store is what a second process reads — live, and after the
    // fact.
    let code = {
        let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::new(NdjsonSink::new(std::io::stderr()))];
        let started = datalib_runs::RunStart {
            kind: if shrink.is_some() {
                datalib_runs::RunKind::Shrink
            } else {
                datalib_runs::RunKind::Sync
            },
            reason: "datalib-pipeline, from the command line".to_string(),
        };
        match host::start_record(&data_root, &cfg, &run_id, &now, started) {
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
                    eprintln!("datalib-pipeline: run store unavailable; nothing recorded this run");
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
        // Each time the loop starts, every step whose program takes the
        // verb is asked to prepare before anything else runs
        // (dag/README.md §"Upgrading a root"); the http server does the
        // same when it takes the lock.
        let asked = datalib_dag::supervisor::upgrade::steps_to_ask(&graph);
        runner.plan(&graph, &asked);
        #[allow(clippy::disallowed_macros)]
        for m in runner.prepare(&graph, &asked).await? {
            match &m.answer {
                Ok(true) => eprintln!(
                    "datalib-pipeline: {} needs to run again before it is up to date; sync it",
                    m.step
                ),
                Ok(false) => {}
                Err(PrepareError::Refused(why)) => eprintln!(
                    "datalib-pipeline: {} refused to prepare, and is turned off until it no longer \
                     does: {why}",
                    m.step
                ),
                Err(PrepareError::Failed(error)) => {
                    eprintln!("datalib-pipeline: could not prepare {}: {error}", m.step)
                }
            }
        }
        if !reset.is_empty() {
            runner.reset(&graph, &reset).await?;
        }
        let shrink_failed = match shrink {
            Some(how) => shrink_and_report(&runner, &graph, &data_root, how).await?,
            None => false,
        };
        let Some(own) = own else {
            store.close().await;
            if shrink_failed {
                std::process::exit(2);
            }
            return Ok(());
        };
        let report = runner.serve(&graph, &store).await?;

        #[allow(clippy::disallowed_macros)]
        for s in &report.steps {
            println!("{}", s.line());
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
                     see the diagnostics above, or run `datalib-pipeline check {}`",
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
    let mut listener = datalib_dag::supervisor::announce::Listener::new(store, "datalib-pipeline");
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
                            "datalib-pipeline: the loop on {} is already running{}; \
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

/// `free-disk-space`, and one line per step whose tree it shrank.
/// The trees are measured from outside, as directories, since the runner
/// never opens a step's store. `true` when some step could not do it.
async fn shrink_and_report(
    runner: &Runner,
    graph: &Graph,
    data_root: &Path,
    how: Shrink,
) -> Result<bool> {
    let tree = |step: &str| tree_bytes(&data_root.join(step));
    let before: HashMap<String, u64> = graph
        .steps
        .iter()
        .filter(|spec| spec.takes(how.verb()))
        .map(|spec| (spec.id.clone(), tree(&spec.id)))
        .collect();
    let mut failed = false;
    let (mut was, mut is) = (0, 0);
    #[allow(clippy::disallowed_macros)]
    for c in runner.shrink(graph, how).await? {
        match c.answer {
            Ok(()) => {
                let (from, to) = (before[&c.step], tree(&c.step));
                if to < from {
                    println!("{:<40} {} → {}", c.step, human_bytes(from), human_bytes(to));
                    (was, is) = (was + from, is + to);
                }
            }
            Err(error) => {
                failed = true;
                println!("{:<40} could not shrink: {error}", c.step);
            }
        }
    }
    #[allow(clippy::disallowed_macros)]
    {
        if was == 0 {
            println!("nothing to give back");
        } else {
            println!(
                "{:<40} {} → {}",
                "in all",
                human_bytes(was),
                human_bytes(is)
            );
        }
    }
    Ok(failed)
}

/// The bytes of every file under `dir`, links not followed; 0 for one
/// that is not there.
fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn acquire_or_explain(data_root: &Path, verb: &str) -> Result<datalib_dag::lock::RunnerLock> {
    datalib_dag::lock::acquire_runner(data_root).map_err(|e| {
        if e.is_held() {
            anyhow::anyhow!(
                "the loop on {} is running{}, and {verb} needs the root to itself. \
                 Quit the app, or wait for the sync to finish.\n(lock: {})",
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

/// `datalib-pipeline serve`: the loop until a signal stops it. Its events go
/// to stderr as NDJSON, its own log lines to the run store as a `dag`
/// process outside any run. A root with no config yet, or one it cannot
/// read, is served all the same: the loop runs nothing until the file
/// holds a config, the way the app's first-run screen needs.
async fn serve(
    config_path: &Path,
    run: datalib_dag::cli::StepArgs,
    stop_tx: &Arc<tokio::sync::watch::Sender<bool>>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let (checked, data_root) = if config_path.is_file() {
        let (checked, data_root) = load_config(config_path)?;
        (Some(checked).filter(|c| !c.is_fatal()), data_root)
    } else {
        let dir = config_path
            .parent()
            .filter(|p| p.is_dir())
            .with_context(|| format!("{} is not in a directory", config_path.display()))?;
        (None, std::path::absolute(dir)?)
    };
    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("create data_root {}", data_root.display()))?;

    let commit = datalib_runs::git_hash_and_origin();
    let retention = checked
        .as_ref()
        .and_then(|c| c.cfg.run_history)
        .map(|h| h.retention())
        .unwrap_or_default();
    let log = datalib_runs::ProcessLogWriter::start(
        &data_root,
        datalib_runs::Process::Dag,
        commit.as_ref().map(|(hash, _)| hash.clone()),
        retention,
    )
    .map(Arc::new);
    // stderr is the event stream, which a fmt layer would interleave
    // prose into; the lines go to the store alone.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        checked
            .as_ref()
            .map(|c| c.cfg.log_filter())
            .unwrap_or_else(|| datalib_runs::filter_at(datalib_runs::DEFAULT_LEVEL))
    });
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(&filter))
        .with(
            log.as_ref()
                .map(|w| datalib_runs::StoreLayer::new(Arc::downgrade(w))),
        )
        .try_init();
    datalib_runs::log_build_commit(commit.as_ref());

    let events = Arc::new(std::sync::Mutex::new(std::io::stderr()));
    let emit: datalib_dag::supervisor::serve::Emit = Arc::new(move |event| {
        let mut w = events.lock().unwrap_or_else(|e| e.into_inner());
        // Best-effort, as every event stream here: a parent that stopped
        // reading costs it the news, not the loop.
        if serde_json::to_writer(&mut *w, &event).is_ok() {
            let _ = std::io::Write::write_all(&mut *w, b"\n");
            let _ = std::io::Write::flush(&mut *w);
        }
    });
    {
        let (emit, runs) = (emit.clone(), datalib_runs::runs_path(&data_root));
        datalib_runs::after_every_write(move |store| {
            if store == runs {
                emit(datalib_dag::supervisor::serve::ServeEvent::RunStoreWritten);
            }
        });
    }

    // The first SIGINT or SIGTERM stops the loop as the app's shutdown
    // does: steps interrupted, requests left open. A second exits at once.
    let stop = stop_tx.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
        let mut signals = 0u32;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
                _ = sighup.recv() => {
                    // No terminal is left to send a second signal.
                    let _ = stop.send(true);
                    tokio::time::sleep(PARENT_GONE_GRACE).await;
                    subprocess::kill_children();
                    std::process::exit(130);
                }
            }
            signals += 1;
            if signals >= 2 {
                subprocess::kill_children();
                std::process::exit(130);
            }
            let _ = stop.send(true);
        }
    });

    let opts = datalib_dag::supervisor::serve::ServeOptions {
        root: data_root,
        binary_dir: run.binary_dir,
        now: run.now,
        parallelism: run.parallelism.map(|n| n.get()),
    };
    datalib_dag::supervisor::serve::run(&opts, emit, stop_rx).await;
    if let Some(log) = log {
        log.flush().await;
    }
    Ok(())
}

async fn run_verb(verb: Verb) -> Result<()> {
    let root = match &verb {
        Verb::Init { data_root } => return init(data_root),
        Verb::Status { root }
        | Verb::Stop { root, .. }
        | Verb::TurnOff { root, .. }
        | Verb::TurnOn { root, .. } => root,
        Verb::Check { .. }
        | Verb::Sync { .. }
        | Verb::Reset { .. }
        | Verb::FreeDiskSpace { .. }
        | Verb::Serve { .. } => unreachable!("main runs these itself"),
    };
    let config_path = datalib_dag::cli::config_path(root);
    let config_path = config_path.as_path();
    let (checked, data_root) = load_config(config_path)?;
    if checked.is_fatal() {
        bail!(
            "{} is not a config: nothing in it could be read",
            config_path.display()
        );
    }
    let store = Store::open(&data_root).await?;
    let said = match &verb {
        Verb::Status { .. } => status_lines(&store, &data_root).await?,
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
        Verb::Init { .. }
        | Verb::Check { .. }
        | Verb::Sync { .. }
        | Verb::Reset { .. }
        | Verb::FreeDiskSpace { .. }
        | Verb::Serve { .. } => unreachable!("returned above"),
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

/// A config that is not there is most often a root not set up yet, so
/// the error names the command that sets one up.
fn load_config(path: &Path) -> Result<(config::ConfigCheck, std::path::PathBuf)> {
    if !path.exists() {
        let root = match (path.file_name(), path.parent()) {
            (Some(name), Some(dir)) if name == config::CONFIG_FILE_NAME => {
                dir.display().to_string()
            }
            _ => "<data_root>".to_string(),
        };
        bail!(
            "{} does not exist. `datalib-pipeline init {root}` writes a starter config there.",
            path.display()
        );
    }
    config::load_graded(path)
}

#[allow(clippy::disallowed_macros)]
fn init(data_root: &Path) -> Result<()> {
    let path = config::root_config_path(data_root);
    if datalib_dag::starter::write_starter_config(data_root)
        .with_context(|| format!("write {}", path.display()))?
    {
        println!("wrote a config with no steps yet: {}", path.display());
    } else {
        println!("{} already exists; left as it is", path.display());
    }
    Ok(())
}

/// What `status` prints when there is nothing to list. It starts with none
/// of the other lines' first words, and never says "running": agents waited
/// with `grep -q running`, which matched the idle line and looped forever.
const IDLE: &str = "idle: no open requests, no steps turned off, nothing owed";

/// One line per open request, per step turned off, per step running and
/// per step at rest that still owes work, in a shape a script can split
/// on two spaces.
async fn status_lines(store: &Store, data_root: &Path) -> Result<Vec<String>> {
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
    let running = store.running_invocations().await?;
    for inv in &running {
        lines.push(format!(
            "running {}  since {}  in run {}",
            inv.step, inv.started_at_utc, inv.run_id
        ));
    }
    // A running step's own queue says what is ahead of it now.
    let at_rest = |step: &str| running.iter().all(|inv| inv.step != step);
    for owed in datalib_runs::latest_metric(data_root, datalib_metrics::OWED).await {
        if owed.value > 0 && at_rest(&owed.step) {
            lines.push(format!(
                "owed {}  {}  as of run {}",
                owed.step, owed.value, owed.run_id
            ));
        }
    }
    if lines.is_empty() {
        lines.push(IDLE.to_string());
    }
    Ok(lines)
}
