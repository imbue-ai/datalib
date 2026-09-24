//! The scenario harness for the loop's management of processes: a data
//! root, a config of puppet steps (`tests/driver/main.rs`), the supervisor
//! loop run in-process as the server runs it, and a person at the controls
//! — sync, stop, pause, resume, edit the config — written to the same store
//! the UI writes. Steps are opaque here: what they write is not checked,
//! only what the loop does with them.
//!
//! Nothing here sleeps. Every wait is for something observable — a
//! driver's ack, an event from the loop, a commit to the store — under a
//! deadline, so a hang fails naming what never came.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use datalib_dag::config::load_graded;
use datalib_dag::scheduler::RetryPolicy;
use datalib_dag::supervisor::announce::{self, Listener};
use datalib_dag::supervisor::record::{Record, StepRecord};
use datalib_dag::supervisor::reload::ConfigFile;
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_dag::{Event, EventSink, Runner};
use tokio::net::unix::pipe;
use tokio::sync::{broadcast, watch};

/// Long enough for a loaded CI runner, short enough that a hang reads as
/// one. Every wait is under it; none waits it out when things work.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// How long a stopped step has before it is killed. Only a step told to
/// ignore its stop ever waits it out.
const STOP_GRACE: Duration = Duration::from_millis(200);

/// What a scenario may set about the loop it runs against.
#[derive(Clone, Copy)]
pub struct Options {
    /// Before a failed attempt's retry. Zero unless a scenario is about
    /// the wait itself.
    pub backoff: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            backoff: Duration::ZERO,
        }
    }
}

/// One `[[steps]]` entry: a driver step, and what it reads.
#[derive(Clone)]
pub struct StepDef {
    pub id: String,
    pub inputs: Vec<String>,
}

pub fn step(id: &str) -> StepDef {
    StepDef {
        id: id.into(),
        inputs: vec![],
    }
}

pub fn reads(id: &str, inputs: &[&str]) -> StepDef {
    StepDef {
        id: id.into(),
        inputs: inputs.iter().map(|s| s.to_string()).collect(),
    }
}

/// The two FIFOs a step's driver talks to the harness through.
struct Ctl {
    /// Held read-write, so the driver's open never waits and a line
    /// written before any driver runs is kept for the first that does.
    instructions: File,
    acks: pipe::Receiver,
    _acks_writer: pipe::Sender,
    read: Vec<u8>,
    lines: VecDeque<String>,
    /// The process that last said it started.
    pid: Option<i32>,
}

/// Every event the loop emits, fanned out to whoever is waiting.
struct Tap(broadcast::Sender<Event>);

impl EventSink for Tap {
    fn emit(&self, event: &Event) {
        let _ = self.0.send(event.clone());
    }
}

pub struct Harness {
    pub root: tempfile::TempDir,
    control: PathBuf,
    driver: PathBuf,
    config: PathBuf,
    steps: BTreeMap<String, Ctl>,
    pub store: Store,
    listener: Listener,
    pub events: broadcast::Receiver<Event>,
    stop: watch::Sender<bool>,
    host: tokio::task::JoinHandle<()>,
    /// Everything the harness did and heard, in order, for a failure or a
    /// hang to print.
    pub trail: Arc<std::sync::Mutex<Vec<String>>>,
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid path and a mode.
    assert_eq!(
        unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
        0,
        "mkfifo {}",
        path.display()
    );
}

fn driver_bin() -> PathBuf {
    let rel = std::env::var("DRIVER_BIN").expect("DRIVER_BIN, from the BUILD rule");
    std::fs::canonicalize(&rel).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

impl Harness {
    pub async fn new(steps: &[StepDef]) -> Harness {
        Harness::with(steps, Options::default()).await
    }

    pub async fn with(steps: &[StepDef], options: Options) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let control = root.path().join("control");
        std::fs::create_dir_all(&control).unwrap();
        let config = root.path().join("config.toml");
        let store = Store::open(root.path()).await.unwrap();
        let listener = Listener::new(&store, "harness").await;
        let (tx, events) = broadcast::channel(4096);
        let (stop, stop_rx) = watch::channel(false);
        let mut h = Harness {
            control,
            driver: driver_bin(),
            config: config.clone(),
            steps: BTreeMap::new(),
            store,
            listener,
            events,
            stop,
            host: tokio::spawn(std::future::pending()),
            trail: Arc::default(),
            root,
        };
        h.set_config(steps);
        h.host.abort();
        h.host = tokio::spawn(host(
            h.root.path().to_path_buf(),
            config,
            Arc::new(Tap(tx)),
            stop_rx,
            options,
        ));
        h
    }

    /// Write the config, as a person saving it from the editor does, and
    /// give each new step its FIFOs.
    pub fn set_config(&mut self, steps: &[StepDef]) {
        let mut text = String::new();
        for s in steps {
            text.push_str(&format!(
                "[[steps]]\nid = {id:?}\ncommand = {cmd:?}\nenv = {{ DRIVER_CONTROL = {ctl:?} }}\ninputs = {inputs:?}\n\n",
                id = s.id,
                cmd = format!("'{}'", self.driver.display()),
                ctl = self.control.display().to_string(),
                inputs = s.inputs,
            ));
            if !self.steps.contains_key(&s.id) {
                self.steps.insert(s.id.clone(), self.ctl(&s.id));
            }
        }
        let tmp = self.config.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, &self.config).unwrap();
        // What the server's watch of `config.toml` does.
        let root = self.config.parent().expect("a config in the root");
        announce::announce(&announce::listeners_dir(root), "config changed");
    }

    fn ctl(&self, id: &str) -> Ctl {
        let key = id.replace('/', "__");
        let ins = self.control.join(format!("{key}.in"));
        let acks = self.control.join(format!("{key}.acks"));
        mkfifo(&ins);
        mkfifo(&acks);
        let instructions = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&ins)
            .unwrap();
        let rx = pipe::OpenOptions::new().open_receiver(&acks).unwrap();
        let writer = pipe::OpenOptions::new().open_sender(&acks).unwrap();
        Ctl {
            instructions,
            acks: rx,
            _acks_writer: writer,
            read: Vec::new(),
            lines: VecDeque::new(),
            pid: None,
        }
    }

    // ── the person at the controls ───────────────────────────────────

    pub fn log(&self, line: String) {
        self.trail.lock().unwrap().push(line);
    }

    pub async fn sync(&self, roots: &[&str]) -> String {
        let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
        let id = self.store.open_request(&roots, "harness").await.unwrap();
        self.log(format!("sync {roots:?} -> {id}"));
        id
    }

    pub async fn stop(&self, request: &str) {
        self.log(format!("stop {request}"));
        self.store.request_stop(request, "harness").await.unwrap();
    }

    pub async fn pause(&self, step: &str) {
        self.log(format!("pause {step}"));
        self.store.pause(step, "harness").await.unwrap();
    }

    pub async fn resume(&self, step: &str) {
        self.log(format!("resume {step}"));
        self.store.resume(step).await.unwrap();
    }

    // ── a step's next moves ──────────────────────────────────────────

    /// Queue an instruction for `step`'s driver, this invocation or the
    /// next one to start.
    pub fn tell(&mut self, step: &str, instruction: &str) {
        self.log(format!("tell {step}: {instruction}"));
        let ctl = self
            .steps
            .get_mut(step)
            .unwrap_or_else(|| panic!("no step {step}"));
        ctl.instructions
            .write_all(format!("{instruction}\n").as_bytes())
            .unwrap();
    }

    /// The next ack from `step`'s driver, in order.
    pub async fn ack(&mut self, step: &str) -> String {
        let ctl = self
            .steps
            .get_mut(step)
            .unwrap_or_else(|| panic!("no step {step}"));
        let prefix = format!("{step} ");
        let wait = async {
            loop {
                if let Some(line) = ctl.lines.pop_front() {
                    return line;
                }
                ctl.acks.readable().await.unwrap();
                let mut buf = [0u8; 4096];
                match ctl.acks.try_read(&mut buf) {
                    Ok(n) => ctl.read.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => panic!("reading {step}'s acks: {e}"),
                }
                while let Some(at) = ctl.read.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = ctl.read.drain(..=at).collect();
                    let line = String::from_utf8(line).unwrap();
                    ctl.lines.push_back(
                        line.trim_end()
                            .strip_prefix(&prefix)
                            .unwrap_or_else(|| panic!("an ack not from {step}: {line}"))
                            .to_string(),
                    );
                }
            }
        };
        let got = tokio::time::timeout(DEADLINE, wait)
            .await
            .unwrap_or_else(|_| panic!("{step} acknowledged nothing within {DEADLINE:?}"));
        self.log(format!("ack {step}: {got}"));
        self.one_at_a_time(step, &got);
        got
    }

    /// One instance of a step at a time: the loop has reaped a step's last
    /// process before it starts the next, so a start finds the last gone.
    fn one_at_a_time(&mut self, step: &str, ack: &str) {
        let Some(pid) = ack
            .split_once("pid=")
            .and_then(|(_, p)| p.parse::<i32>().ok())
        else {
            return;
        };
        let ctl = self.steps.get_mut(step).expect("a step we have");
        if let Some(last) = ctl.pid.replace(pid) {
            // SAFETY: signal 0 only asks whether the process exists.
            let alive = unsafe { libc::kill(last, 0) } == 0;
            assert!(
                !alive,
                "{step} started as {pid} while {last} was still running"
            );
        }
    }

    /// Every ack `step`'s driver has written and nobody has read, without
    /// waiting for more.
    pub fn pending_acks(&mut self, step: &str) -> Vec<String> {
        let ctl = self
            .steps
            .get_mut(step)
            .unwrap_or_else(|| panic!("no step {step}"));
        let mut buf = [0u8; 4096];
        while let Ok(n) = ctl.acks.try_read(&mut buf) {
            if n == 0 {
                break;
            }
            ctl.read.extend_from_slice(&buf[..n]);
        }
        let prefix = format!("{step} ");
        while let Some(at) = ctl.read.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = ctl.read.drain(..=at).collect();
            let line = String::from_utf8(line).unwrap();
            ctl.lines.push_back(
                line.trim_end()
                    .strip_prefix(&prefix)
                    .unwrap_or_else(|| panic!("an ack not from {step}: {line}"))
                    .to_string(),
            );
        }
        let acks: Vec<String> = ctl.lines.drain(..).collect();
        for ack in &acks {
            self.log(format!("ack {step}: {ack}"));
            self.one_at_a_time(step, ack);
        }
        acks
    }

    /// Wait for `step`'s next ack and require it to start with `want`.
    pub async fn expect(&mut self, step: &str, want: &str) -> String {
        let got = self.ack(step).await;
        assert!(
            got.starts_with(want),
            "{step}: wanted an ack starting {want:?}, got {got:?}"
        );
        got
    }

    /// Tell, and wait for the ack that says it happened.
    pub async fn done(&mut self, step: &str, instruction: &str) -> String {
        self.tell(step, instruction);
        self.ack(step).await
    }

    // ── what the loop and the store say ──────────────────────────────

    /// Wait until `ready` holds of the store's state, re-checking at every
    /// commit to the store and no other time.
    pub async fn until<T>(
        &mut self,
        what: &str,
        mut ready: impl FnMut(&Record, &BTreeMap<String, Option<Option<RequestOutcome>>>) -> Option<T>,
    ) -> T {
        let wait = async {
            loop {
                let record = self.store.load_record().await.unwrap();
                let requests = self
                    .store
                    .recent_requests(1000)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.id, r.closed))
                    .collect();
                if let Some(t) = ready(&record, &requests) {
                    return t;
                }
                self.listener.next(&self.store).await;
            }
        };
        tokio::time::timeout(DEADLINE, wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: not within {DEADLINE:?}"))
    }

    /// The next commit to the store, from anyone.
    pub async fn next_commit(&mut self, what: &str) {
        tokio::time::timeout(DEADLINE, self.listener.next(&self.store))
            .await
            .unwrap_or_else(|_| panic!("{what}: no commit within {DEADLINE:?}"));
    }

    pub async fn closed(&mut self, request: &str) -> Option<RequestOutcome> {
        let id = request.to_string();
        self.until(&format!("request {request} to close"), |_, reqs| {
            reqs.get(&id).cloned().flatten()
        })
        .await
    }

    pub async fn record(&self, step: &str) -> StepRecord {
        let record = self.store.load_record().await.unwrap();
        record.steps.get(step).cloned().unwrap_or_default()
    }

    /// The next event matching `want`, skipping the rest.
    pub async fn event(&mut self, what: &str, mut want: impl FnMut(&Event) -> bool) -> Event {
        let wait = async {
            loop {
                match self.events.recv().await {
                    Ok(e) if want(&e) => return e,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        panic!("{n} events lost waiting for {what}")
                    }
                    Err(e) => panic!("the loop's events ended waiting for {what}: {e}"),
                }
            }
        };
        tokio::time::timeout(DEADLINE, wait)
            .await
            .unwrap_or_else(|_| panic!("no {what} within {DEADLINE:?}"))
    }

    /// Stop the loop and require that nothing it did needed its backstop.
    pub async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(DEADLINE, self.host).await;
        assert_eq!(
            announce::missed_announcements(),
            0,
            "a commit reached some listener only through its backstop: nobody announced it"
        );
    }
}

/// The loop as the server runs it: a busy period whenever a request is
/// open, woken by commits to the store and nothing else.
async fn host(
    root: PathBuf,
    config: PathBuf,
    sink: Arc<dyn EventSink>,
    mut stop: watch::Receiver<bool>,
    options: Options,
) {
    let store = Store::open(&root).await.unwrap();
    let mut listener = Listener::new(&store, "harness host").await;
    while !*stop.borrow() {
        let open = store.open_requests().await.unwrap();
        if open.iter().any(|r| r.stop_requested_by.is_none()) {
            let (checked, _) = load_graded(&config).unwrap();
            let mut runner = Runner::new(&root)
                .sink(sink.clone())
                .retry(RetryPolicy {
                    backoff: options.backoff,
                    ..RetryPolicy::default()
                })
                .stop_on(stop.clone())
                .reload_from(Arc::new(ConfigFile::new(&config)));
            runner.stop_grace = STOP_GRACE;
            runner.serve(&checked.graph, &store).await.unwrap();
            continue;
        }
        for r in open {
            store
                .close_request(&r.id, RequestOutcome::Stopped, None)
                .await
                .unwrap();
        }
        tokio::select! {
            _ = listener.next(&store) => {}
            _ = stop.changed() => {}
        }
    }
    store.close().await;
}
