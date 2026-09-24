//! `datalib-dag` — run a DAG config file (see `datalib_dag::config`
//! for the schema).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use datalib_dag::scheduler::ResetTarget;

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
use datalib_dag::supervisor::tick::Budgets;
use datalib_dag::{config, subprocess, EventSink, NdjsonSink, Runner};
use strum::{EnumString, IntoStaticStr};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// After the parent is gone — or the terminal is — how long the steps
/// get to checkpoint on their SIGINT before they are killed: the grace a
/// stopped step gets anywhere. Used where no second signal is coming, so
/// the runner has to escalate on its own.
const PARENT_GONE_GRACE: std::time::Duration = datalib_dag::step::STOP_GRACE;

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
    const USAGE: &str = "usage: datalib-dag <config.toml> [--binary-dir DIR] \
         [--sync STEP_ID[,STEP_ID…]]… [--reset STEP_ID[+blobs][,…]]… [--now RFC3339] \
         [--run-id ID] [--parallelism N] [--by WHO]\n       \
         datalib-dag --check <config.toml>\n\n\
         --reset empties what a step wrote (its store; `+blobs` an ingest step's blob \
         CAS with it), keeping its doltlite history, so the next run does its work \
         from the start. Alone, that is all the invocation does; with --sync it runs \
         first.\n\n\
         A sync is a request in <root>/system/supervisor.sqlite, tagged --by (default \
         `cli`). If another process is already running the loop on this root — the app, \
         or another datalib-dag — this one hands it the request and follows it; either \
         way it exits with the request's outcome. Ctrl-C asks for this request to stop.\n\n\
         datalib-dag status <config.toml>                          open requests, pauses, running steps\n\
         datalib-dag stop <config.toml> <request-id> [--by WHO]    ask a request to stop\n\
         datalib-dag pause <config.toml> <step-id> [--by WHO]      keep a step from running\n\
         datalib-dag resume <config.toml> <step-id>                lift a pause\n\n\
         Each writes a row and returns; whoever runs the loop acts on it within a second.";
    if let Some(verb) = std::env::args().nth(1).as_deref().and_then(Verb::parse) {
        return run_verb(verb, std::env::args().skip(2).collect(), USAGE).await;
    }
    let mut config_path: Option<PathBuf> = None;
    let mut binary_dir: Option<PathBuf> = None;
    let mut sync_only: Vec<String> = Vec::new();
    let mut now: Option<String> = None;
    let mut run_id: Option<String> = None;
    let mut parallelism: Option<usize> = None;
    let mut reset: Vec<ResetTarget> = Vec::new();
    let mut check_only = false;
    let mut by = "cli".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--binary-dir" => {
                binary_dir = Some(PathBuf::from(
                    args.next().context("--binary-dir needs a value")?,
                ))
            }
            "--sync" => {
                let v = args.next().context("--sync needs a step id")?;
                sync_only.extend(v.split(',').map(|s| s.trim().to_string()));
            }
            "--now" => now = Some(args.next().context("--now needs a value")?),
            "--run-id" => run_id = Some(args.next().context("--run-id needs a value")?),
            "--parallelism" => {
                parallelism = Some(
                    args.next()
                        .context("--parallelism needs a value")?
                        .parse()
                        .context("--parallelism must be a positive integer")?,
                )
            }
            "--reset" => {
                let v = args.next().context("--reset needs a step id")?;
                reset.extend(v.split(',').map(|s| ResetTarget::parse(s.trim())));
            }
            "--check" => check_only = true,
            "--by" => by = args.next().context("--by needs a name")?,
            "--version" | "-V" => {
                #[allow(clippy::disallowed_macros)]
                {
                    println!("datalib-dag {VERSION_RESOLVED}");
                }
                return Ok(());
            }
            "-h" | "--help" => {
                // stdout is this tool's interface; no bars in play.
                #[allow(clippy::disallowed_macros)]
                {
                    println!("{USAGE}");
                }
                return Ok(());
            }
            _ if config_path.is_none() => config_path = Some(PathBuf::from(a)),
            other => bail!("unexpected argument {other:?}"),
        }
    }
    let config_path = config_path.context(USAGE)?;
    if let Some(0) = parallelism {
        bail!("--parallelism must be at least 1");
    }

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

    if !sync_only.is_empty() {
        let fringe = graph.fringe_ids();
        for id in &sync_only {
            if !fringe.contains(&id.as_str()) {
                bail!(
                    "--sync {id:?}: not a source step (a step with no inputs). \
                     Available: {}",
                    fringe.join(", ")
                );
            }
        }
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
            runner.budgets = Budgets::from_parallelism(p);
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
    Lock(datalib_dag::lock::FileLock),
    /// Another process ran it, and the request is over.
    Closed(Option<RequestOutcome>),
}

/// Take the loop, or follow the request while someone else runs it. The
/// lock is tried again on every beat, not just once: the loop that was
/// running may end between our look and our request landing, and then
/// nobody is left to serve it but us.
async fn follow_or_lock(data_root: &Path, store: &Store, own: &str) -> Result<Taken> {
    const BEAT: std::time::Duration = std::time::Duration::from_millis(500);
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
                tokio::time::sleep(BEAT).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn acquire_or_explain(data_root: &Path) -> Result<datalib_dag::lock::FileLock> {
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

/// The steering verbs: each writes intent into the store and returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Verb {
    Status,
    Stop,
    Pause,
    Resume,
}

impl Verb {
    fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

async fn run_verb(verb: Verb, args: Vec<String>, usage: &str) -> Result<()> {
    let mut positional: Vec<String> = Vec::new();
    let mut by = "cli".to_string();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--by" => by = args.next().context("--by needs a name")?,
            _ if a.starts_with("--") => bail!("unexpected argument {a:?}\n\n{usage}"),
            _ => positional.push(a),
        }
    }
    let wants = if verb == Verb::Status { 1 } else { 2 };
    if positional.len() != wants {
        bail!(
            "{} takes {wants} argument(s)\n\n{usage}",
            <&str>::from(verb)
        );
    }
    let (checked, data_root) = config::load_graded(Path::new(&positional[0]))?;
    if checked.is_fatal() {
        bail!(
            "{} is not a config: nothing in it could be read",
            positional[0]
        );
    }
    let store = Store::open(&data_root).await?;
    let said = match verb {
        Verb::Status => status_lines(&store).await?,
        Verb::Stop => {
            let id = &positional[1];
            match store.request(id).await?.map(|r| r.closed) {
                None => bail!("no request {id} in {}", data_root.display()),
                Some(Some(outcome)) => vec![format!(
                    "request {id} is already over: {}",
                    outcome.map(RequestOutcome::as_str).unwrap_or("closed")
                )],
                Some(None) => {
                    store.request_stop(id, &by).await?;
                    vec![format!("asked request {id} to stop")]
                }
            }
        }
        Verb::Pause => {
            let step = &positional[1];
            if !checked.graph.by_id.contains_key(step) {
                let mut ids: Vec<&str> = checked.graph.by_id.keys().map(String::as_str).collect();
                ids.sort();
                bail!(
                    "no step {step:?} in this config; its steps: {}",
                    ids.join(", ")
                );
            }
            match store.paused().await?.get(step) {
                Some(who) => vec![format!("{step} is already paused, by {who}")],
                None => {
                    store.pause(step, &by).await?;
                    vec![format!("paused {step}")]
                }
            }
        }
        Verb::Resume => {
            let step = &positional[1];
            match store.paused().await?.get(step) {
                None => vec![format!("{step} was not paused")],
                Some(who) => {
                    store.resume(step).await?;
                    vec![format!("resumed {step}, which {who} had paused")]
                }
            }
        }
    };
    store.close().await;
    #[allow(clippy::disallowed_macros)]
    for line in said {
        println!("{line}");
    }
    Ok(())
}

/// One line per open request and per pause, in a shape a script can split
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
    for (step, who) in store.paused().await? {
        lines.push(format!("paused {step}  by {who}"));
    }
    for inv in store.running_invocations().await? {
        lines.push(format!(
            "running {}  since {}  in run {}",
            inv.step, inv.started_at_utc, inv.run_id
        ));
    }
    if lines.is_empty() {
        lines.push("nothing open, nothing paused, nothing running".to_string());
    }
    Ok(lines)
}
