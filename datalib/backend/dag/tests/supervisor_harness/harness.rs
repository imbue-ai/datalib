//! The loop, run in-process by the one idle host (`supervisor::host::
//! run_idle`), over a real `config.toml` of puppet steps; and a person at
//! its controls, through the store. Everything the harness waits on is
//! observable — a puppet's ack, a loop event, an announcement — and every
//! wait has a deadline that names what never came.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use datalib_dag::config;
use datalib_dag::scheduler::RetryPolicy;
use datalib_dag::supervisor::announce::{
    announce, missed_announcements, Listener, CONFIG_CHANGED, FROM_SERVER,
};
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::record::{InvocationEnd, InvocationRow, Record};
use datalib_dag::supervisor::reload::ConfigFile;
use datalib_dag::supervisor::store::{RequestOutcome, RequestRow, Store};
use datalib_dag::{Event, EventSink, Runner};
use tokio::sync::{mpsc, watch};

const DEADLINE: Duration = Duration::from_secs(10);
const TRAIL: usize = 40;

/// The product clocks a scenario depends on, each set by the scenario.
#[derive(Clone, Copy, Debug)]
pub struct Clocks {
    pub stop_grace: Duration,
    pub backoff: Duration,
    pub backstop: Duration,
}

impl Default for Clocks {
    /// Nothing here should fire unless a scenario means it to: a stop is
    /// honoured long before its grace, and a retry waits an hour.
    fn default() -> Self {
        Clocks {
            stop_grace: Duration::from_secs(30),
            backoff: Duration::from_secs(3600),
            backstop: Duration::from_millis(100),
        }
    }
}

/// A step in the harness's config: a puppet, reading `inputs`, holding
/// `locks` (as the config writes them, `locks = …`) and reading as
/// `reads` says.
#[derive(Clone, Debug, Default)]
pub struct Step {
    pub id: String,
    pub inputs: Vec<String>,
    pub locks: Option<String>,
    pub reads: Option<&'static str>,
}

pub fn source(id: &str) -> Step {
    Step {
        id: id.into(),
        ..Step::default()
    }
}

pub fn reads(id: &str, inputs: &[&str]) -> Step {
    Step {
        id: id.into(),
        inputs: inputs.iter().map(|s| s.to_string()).collect(),
        ..Step::default()
    }
}

impl Step {
    /// `locks = <toml>`, as written: `["gpu"]`, `{ gpu = "exclusive" }`.
    pub fn locks(mut self, toml: &str) -> Step {
        self.locks = Some(toml.into());
        self
    }

    /// Reads its inputs off disk, so no writer of them runs beside it.
    pub fn reads_files(mut self) -> Step {
        self.reads = Some("files");
        self
    }
}

/// `[[locks]]` entries every harness config has: the default budgets wide
/// enough that no scenario waits on one it does not mean to.
const WIDE_DEFAULTS: &str = "[[locks]]\nname = \"network\"\nslots = 8\n\n\
                             [[locks]]\nname = \"cpu\"\nslots = 8\n\n\
                             [[locks]]\nname = \"index\"\nslots = 8\n\n";

#[derive(Debug, Clone)]
pub enum Seen {
    /// `<step> <pid> <what>` from a puppet.
    Ack {
        step: String,
        pid: i32,
        what: String,
    },
    Event(Event),
    /// Something announced to the store's listeners.
    Heard(String),
    /// What the host did: a busy period, a settle.
    Host(String),
    /// The loop returned an error.
    Broke(String),
}

/// What the store says now: the record, every invocation, and requests.
pub struct State {
    pub record: Record,
    pub invocations: Vec<(InvocationRow, Option<InvocationEnd>)>,
    pub requests: Vec<RequestRow>,
}

impl State {
    pub fn ended(&self, step: &str) -> Vec<&InvocationEnd> {
        self.invocations
            .iter()
            .filter(|(row, _)| row.step == step)
            .filter_map(|(_, end)| end.as_ref())
            .collect()
    }

    pub fn started(&self, step: &str) -> usize {
        self.invocations
            .iter()
            .filter(|(r, _)| r.step == step)
            .count()
    }

    pub fn request(&self, id: &str) -> Option<&RequestRow> {
        self.requests.iter().find(|r| r.id == id)
    }

    pub fn outcome(&self, id: &str) -> Option<Option<RequestOutcome>> {
        self.request(id).and_then(|r| r.closed)
    }

    /// The version the loop recorded for what `step` writes.
    pub fn version(&self, step: &str) -> Option<&str> {
        self.record.steps.get(step)?.version.as_deref()
    }

    pub fn detail(&self, step: &str) -> Option<&str> {
        self.record.steps.get(step)?.state_detail.as_deref()
    }
}

struct ChannelSink(mpsc::UnboundedSender<Seen>);

impl EventSink for ChannelSink {
    fn emit(&self, event: &Event) {
        let _ = self.0.send(Seen::Event(event.clone()));
    }
}

pub struct Harness {
    pub root: tempfile::TempDir,
    puppets: PathBuf,
    puppet_bin: String,
    person: Store,
    rx: mpsc::UnboundedReceiver<Seen>,
    trail: VecDeque<String>,
    seen_count: usize,
    /// Seen, and not yet taken by a wait: the next wait for something
    /// takes the earliest that matches, so two things arriving in the
    /// other order than they are waited for are both found.
    backlog: VecDeque<Seen>,
    /// Said first when a check fails: the walk's seed, say.
    pub context: String,
    /// The scenario's own `[[locks]]` entries, written into every config.
    locks: String,
    fifos: HashMap<String, (File, File)>,
    pids: HashMap<String, Vec<i32>>,
    stop: watch::Sender<bool>,
    host: Option<tokio::task::JoinHandle<()>>,
    _acks: File,
}

impl Harness {
    pub async fn new(steps: &[Step]) -> Harness {
        Harness::with(steps, Clocks::default()).await
    }

    pub async fn with(steps: &[Step], clocks: Clocks) -> Harness {
        Harness::with_locks(steps, clocks, "").await
    }

    /// With `[[locks]]` entries of the scenario's own, as TOML.
    pub async fn with_locks(steps: &[Step], clocks: Clocks, locks: &str) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let puppets = root.path().join("puppets");
        std::fs::create_dir_all(&puppets).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let acks_path = puppets.join("acks");
        mkfifo(&acks_path);
        let acks = std::fs::OpenOptions::new()
            .read(true)
            .open_nonblocking(&acks_path);
        // Our own write end, so the reader never sees end-of-file between
        // puppets.
        let hold = std::fs::OpenOptions::new()
            .write(true)
            .open_nonblocking(&acks_path);
        let ack_tx = tx.clone();
        std::thread::spawn(move || read_acks(acks, ack_tx));

        let person = Store::open(root.path()).await.unwrap();
        let ear = Store::open(root.path()).await.unwrap();
        let mut listener = Listener::new(&ear, "the harness").await;
        let heard_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                for line in listener.next(&ear).await {
                    if heard_tx.send(Seen::Heard(line)).is_err() {
                        return;
                    }
                }
            }
        });

        let mut h = Harness {
            puppets,
            // Absolute: a step runs in the data root, not here.
            puppet_bin: std::fs::canonicalize(std::env::var("PUPPET").expect("PUPPET"))
                .unwrap()
                .display()
                .to_string(),
            person,
            rx,
            trail: VecDeque::new(),
            seen_count: 0,
            backlog: VecDeque::new(),
            context: String::new(),
            locks: locks.to_string(),
            fifos: HashMap::new(),
            pids: HashMap::new(),
            stop: watch::channel(false).0,
            host: None,
            _acks: hold,
            root,
        };
        h.write_config(steps);
        h.start_host(tx, clocks).await;
        h
    }

    async fn start_host(&mut self, tx: mpsc::UnboundedSender<Seen>, clocks: Clocks) {
        let root = self.root.path().to_path_buf();
        let mut stop = self.stop.subscribe();
        let store = Store::open(&root).await.unwrap();
        let mut listener = Listener::new(&store, "the loop's host")
            .await
            .backstop(clocks.backstop);
        let mut periods = Periods {
            root,
            report: tx.clone(),
            sink: Arc::new(ChannelSink(tx)),
            clocks,
            stop: stop.clone(),
        };
        self.host = Some(tokio::spawn(async move {
            host::run_idle(&store, &mut listener, &mut periods, &mut stop).await;
            store.close().await;
        }));
    }

    fn config_text(&self, steps: &[Step], locks: &str) -> String {
        let mut text = format!("{WIDE_DEFAULTS}{locks}");
        for s in steps {
            let inputs: Vec<String> = s.inputs.iter().map(|i| format!("{i:?}")).collect();
            text.push_str(&format!(
                "[[steps]]\nid = {:?}\ncommand = {:?}\ninputs = [{}]\n",
                s.id,
                self.puppet_bin,
                inputs.join(", "),
            ));
            if let Some(locks) = &s.locks {
                text.push_str(&format!("locks = {locks}\n"));
            }
            if let Some(reads) = s.reads {
                text.push_str(&format!("reads = {reads:?}\n"));
            }
            text.push_str(&format!(
                "[steps.env]\nPUPPET_DIR = {:?}\n\n",
                self.puppets.display().to_string()
            ));
        }
        text
    }

    fn write_config(&mut self, steps: &[Step]) {
        for s in steps {
            self.fifo(&s.id);
        }
        let path = config::root_config_path(self.root.path());
        let tmp = path.with_extension("tmp");
        let text = self.config_text(steps, &self.locks);
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
    }

    /// A config edit, announced as the server's watch does.
    pub fn edit_config(&mut self, steps: &[Step]) {
        self.write_config(steps);
        announce(
            &datalib_dag::supervisor::announce::listeners_dir(self.root.path()),
            FROM_SERVER,
            CONFIG_CHANGED,
        );
        self.note(format!("person: edit config to {:?}", ids(steps)));
    }

    fn fifo(&mut self, step: &str) -> &mut File {
        let puppets = self.puppets.clone();
        let (_, tx) = self.fifos.entry(step.to_string()).or_insert_with(|| {
            let path = puppets.join(format!("{}.in", step.replace('/', "__")));
            mkfifo(&path);
            // A read end nobody reads, held so the write end opens and
            // lines wait in the pipe for whichever puppet runs next.
            let read = std::fs::OpenOptions::new()
                .read(true)
                .open_nonblocking(&path);
            let write = std::fs::OpenOptions::new()
                .write(true)
                .open_nonblocking(&path);
            (read, write)
        });
        tx
    }

    fn note(&mut self, line: String) {
        self.seen_count += 1;
        if self.trail.len() == TRAIL {
            self.trail.pop_front();
        }
        self.trail.push_back(line);
    }

    pub fn trail(&self) -> String {
        self.trail.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Queue an instruction for `step`'s puppet, running or next to run.
    pub fn tell(&mut self, step: &str, instruction: &str) {
        let line = format!("{instruction}\n");
        self.fifo(step).write_all(line.as_bytes()).unwrap();
        self.note(format!("tell {step}: {instruction}"));
    }

    /// Tell, and wait until the puppet says it has done it.
    pub async fn run(&mut self, step: &str, instruction: &str) {
        self.tell(step, instruction);
        let want = format!("did {instruction}");
        self.ack(step, &want).await;
    }

    /// Wait for `step`'s puppet to ack `what` (a prefix); its pid.
    pub async fn ack(&mut self, step: &str, what: &str) -> i32 {
        let desc = format!("{step} to ack {what:?}");
        self.wait(&desc, |s| match s {
            Seen::Ack {
                step: st,
                pid,
                what: w,
            } if st == step && w.starts_with(what) => Some(*pid),
            _ => None,
        })
        .await
    }

    /// Wait for `step`'s puppet to start; its pid.
    pub async fn started(&mut self, step: &str) -> i32 {
        self.ack(step, "started").await
    }

    /// The earliest thing seen, and not yet taken, for which `pick` says
    /// yes, checking the invariants on everything as it arrives.
    pub async fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Seen) -> Option<T>) -> T {
        if let Some(at) = self.backlog.iter().position(|s| pick(s).is_some()) {
            let seen = self.backlog.remove(at).expect("just found");
            return pick(&seen).expect("just matched");
        }
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let seen = match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(seen)) => seen,
                Ok(None) => self.fail(&format!("the harness's channel closed waiting for {what}")),
                Err(_) => self.fail(&format!("no {what} within {DEADLINE:?}")),
            };
            self.saw(&seen);
            if let Some(t) = pick(&seen) {
                return t;
            }
            self.backlog.push_back(seen);
        }
    }

    /// Every ack that has arrived and no wait has taken, oldest first,
    /// taken now, without waiting for more.
    pub fn take_acks(&mut self) -> Vec<(String, i32, String)> {
        while let Ok(seen) = self.rx.try_recv() {
            self.saw(&seen);
            self.backlog.push_back(seen);
        }
        let mut acks = Vec::new();
        self.backlog.retain(|s| match s {
            Seen::Ack { step, pid, what } => {
                acks.push((step.clone(), *pid, what.clone()));
                false
            }
            _ => true,
        });
        acks
    }

    /// Puppets whose process is still there.
    pub fn live_puppets(&self) -> Vec<(String, i32)> {
        self.pids
            .iter()
            .flat_map(|(s, ps)| ps.iter().map(move |p| (s.clone(), *p)))
            .filter(|(_, p)| alive(*p))
            .collect()
    }

    /// A line of the walk's own in the trail.
    pub fn say(&mut self, line: String) {
        self.note(line);
    }

    fn saw(&mut self, seen: &Seen) {
        let line = match seen {
            Seen::Ack { step, pid, what } => format!("ack {step} [{pid}] {what}"),
            Seen::Event(e) => format!("event {}", describe(e)),
            Seen::Heard(l) => format!("heard {l}"),
            Seen::Host(l) => format!("host: {l}"),
            Seen::Broke(why) => {
                let why = why.clone();
                self.fail(&format!("the loop broke: {why}"))
            }
        };
        self.note(line);
        if let Seen::Ack { step, pid, what } = seen {
            if what.starts_with("started") {
                let pid = *pid;
                let others: Vec<i32> = self.pids.get(step).cloned().unwrap_or_default();
                let alive: Vec<i32> = others.into_iter().filter(|p| alive(*p)).collect();
                if !alive.is_empty() {
                    let step = step.clone();
                    self.fail(&format!(
                        "{step} started as {pid} while {alive:?} of it still ran"
                    ));
                }
                self.pids.insert(step.clone(), vec![pid]);
            }
        }
    }

    pub fn fail(&self, why: &str) -> ! {
        panic!(
            "{}{why}\n--- the last {} of {} things seen ---\n{}",
            self.context,
            self.trail.len(),
            self.seen_count,
            self.trail()
        )
    }

    pub async fn state(&self) -> State {
        State {
            record: self.person.load_record().await.unwrap(),
            invocations: self.person.invocations().await.unwrap(),
            requests: self.person.recent_requests(1000).await.unwrap(),
        }
    }

    /// Wait until what the store says satisfies `check`, looking again
    /// whenever something is announced.
    pub async fn until<T>(&mut self, what: &str, mut check: impl FnMut(&State) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            if let Some(t) = check(&self.state().await) {
                return t;
            }
            let seen = match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(seen)) => seen,
                Ok(None) => self.fail(&format!("the harness's channel closed waiting for {what}")),
                Err(_) => self.fail(&format!("no {what} within {DEADLINE:?}")),
            };
            self.saw(&seen);
            self.backlog.push_back(seen);
        }
    }

    // The person at the controls.

    pub async fn sync(&mut self, roots: &[&str]) -> String {
        let roots: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
        let id = self.person.open_request(&roots, "person").await.unwrap();
        self.note(format!("person: sync {roots:?} = {id}"));
        id
    }

    pub async fn stop(&mut self, request: &str) {
        self.person.request_stop(request, "person").await.unwrap();
        self.note(format!("person: stop {request}"));
    }

    pub async fn pause(&mut self, step: &str) {
        self.person.pause(step, "person").await.unwrap();
        self.note(format!("person: pause {step}"));
    }

    pub async fn resume(&mut self, step: &str) {
        self.person.resume(step).await.unwrap();
        self.note(format!("person: resume {step}"));
    }

    /// Wait for a request to close; how it did.
    pub async fn closed(&mut self, request: &str) -> RequestOutcome {
        let desc = format!("request {request} to close");
        self.until(&desc, |s| s.outcome(request))
            .await
            .unwrap_or_else(|| self.fail(&format!("{request} closed with an outcome unknown")))
    }

    /// Wait for `step`'s `n`th invocation (1-based) to be recorded as
    /// ended; how it did. Written the tick after the invocation closes.
    pub async fn ended(&mut self, step: &str, n: usize) -> InvocationEnd {
        let desc = format!("{step}'s invocation {n} to be recorded as ended");
        self.until(&desc, |s| s.ended(step).get(n - 1).map(|e| (*e).clone()))
            .await
    }

    /// Stop the host as Ctrl-C does, and wait for it to let go.
    pub async fn stop_host(&mut self) {
        let _ = self.stop.send(true);
        if let Some(host) = self.host.take() {
            if tokio::time::timeout(DEADLINE, host).await.is_err() {
                self.fail("the host did not stop within the deadline");
            }
        }
    }

    /// Stop the host, and check what must hold after every
    /// scenario: no puppet left alive, and no commit nobody announced.
    pub async fn finish(mut self) {
        self.stop_host().await;
        let alive = self.live_puppets();
        if !alive.is_empty() {
            self.fail(&format!("puppets outlived the host: {alive:?}"));
        }
        assert_eq!(missed_announcements(), 0, "a commit nobody announced");
    }
}

struct Periods {
    root: PathBuf,
    sink: Arc<dyn EventSink>,
    clocks: Clocks,
    stop: watch::Receiver<bool>,
    report: mpsc::UnboundedSender<Seen>,
}

impl Periods {
    fn graph(&self) -> Option<(config::ConfigCheck, PathBuf)> {
        let path = config::root_config_path(&self.root);
        match config::load_graded(&path) {
            Ok((checked, _)) if !checked.is_fatal() => Some((checked, path)),
            Ok((checked, _)) => {
                let _ = self.report.send(Seen::Broke(checked.render(&path)));
                None
            }
            Err(e) => {
                let _ = self.report.send(Seen::Broke(format!("{e:#}")));
                None
            }
        }
    }
}

impl host::Periods for Periods {
    async fn busy_period(&mut self, store: &Store) {
        let _ = self.report.send(Seen::Host("busy period".into()));
        let Some((checked, path)) = self.graph() else {
            return;
        };
        let run_id = datalib_dag::scheduler::new_run_id();
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs();
        let env = host::step_env(&checked.cfg, None, &[], &now, &run_id).unwrap();
        let mut runner = Runner::new(&self.root)
            .sink(self.sink.clone())
            .child_env(env.vars)
            .stop_on(self.stop.clone())
            .retry(RetryPolicy {
                backoff: self.clocks.backoff,
                ..RetryPolicy::default()
            })
            .reload_from(Arc::new(ConfigFile::new(path)));
        runner.stop_grace = self.clocks.stop_grace;
        runner.backstop = self.clocks.backstop;
        if let Err(e) = runner.serve(&checked.graph, store).await {
            let _ = self.report.send(Seen::Broke(format!("{e:#}")));
        }
        let _ = self.report.send(Seen::Host("busy period over".into()));
    }

    async fn settle(&mut self, store: &Store) -> Option<BTreeMap<String, String>> {
        let (checked, _) = self.graph()?;
        match Runner::new(&self.root).settle(&checked.graph, store).await {
            Ok(paused) => {
                let steps: Vec<&String> = paused.keys().collect();
                let _ = self.report.send(Seen::Host(format!("settled {steps:?}")));
                Some(paused)
            }
            Err(e) => {
                let _ = self.report.send(Seen::Broke(format!("{e:#}")));
                None
            }
        }
    }

    async fn idle_work(&mut self, _: &Store) {}

    async fn nudged(&self) {
        std::future::pending().await
    }
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

fn describe(e: &Event) -> String {
    match e {
        Event::StepStart { step, attempt, .. } => format!("start {step} attempt {attempt}"),
        Event::PassEnd { step, .. } => format!("pass-end {step}"),
        Event::StepFinish {
            step,
            status,
            error,
            ..
        } => format!(
            "finish {step} {status:?} {}",
            error.as_deref().unwrap_or("")
        ),
        Event::Checkpoint { step, version, .. } => format!("checkpoint {step} {version}"),
        Event::Metric {
            step,
            name,
            labels,
            value,
            ..
        } => format!("metric {step} {name}{labels:?} = {value}"),
        Event::Log { step, msg, .. } => format!("log {step}: {msg}"),
        other => format!("{other:?}").chars().take(120).collect(),
    }
}

/// Whether `pid` is a live process (a zombie counts: it is not reaped).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path and a mode.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

trait OpenNonblocking {
    fn open_nonblocking(&mut self, path: &Path) -> File;
}

impl OpenNonblocking for std::fs::OpenOptions {
    fn open_nonblocking(&mut self, path: &Path) -> File {
        self.custom_flags(libc::O_NONBLOCK).open(path).unwrap()
    }
}

/// Every ack line, as `Seen::Ack`, on a thread of its own: a blocking read
/// that needs nothing of the runtime.
fn read_acks(acks: File, tx: mpsc::UnboundedSender<Seen>) {
    use std::io::Read;
    // Blocking from here on: the fd was opened non-blocking only so the
    // open did not wait for a writer.
    // SAFETY: clears O_NONBLOCK on an fd this thread owns.
    unsafe {
        use std::os::fd::AsRawFd;
        let fd = acks.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
    let mut acks = acks;
    let mut partial = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match acks.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        partial.extend_from_slice(&buf[..n]);
        while let Some(at) = partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = partial.drain(..=at).collect();
            let line = String::from_utf8_lossy(&line[..at]).into_owned();
            let mut parts = line.splitn(3, ' ');
            let (Some(step), Some(pid), what) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let seen = Seen::Ack {
                step: step.to_string(),
                pid: pid.parse().unwrap_or(0),
                what: what.unwrap_or("").to_string(),
            };
            if tx.send(seen).is_err() {
                return;
            }
        }
    }
}
