//! A step that does only what it is told, for the supervisor harness.
//!
//! It reads one instruction per line from `$PUPPET_DIR/<step>.in`, a FIFO
//! the harness holds open, one byte at a time: a buffered reader would
//! take the next invocation's instructions with it when this one ends. It
//! acks each on `$PUPPET_DIR/acks` as `<step> <pid> <what>` once it is
//! done. It keeps nothing of its own: the versions the loop tracks are the
//! ones it reports.
//!
//! Instructions: `write <text>`, `fill <bytes>` (incidental IO in its
//! tree); `seal <version> [rows]`; `streams`; `metric <name> <value>`;
//! `progress_len <n>`, `progress_inc <n>`, `progress_msg <text>`;
//! `log <level> <msg>`; `raw <line>`; `reads` (acks `DATALIB_READS`);
//! `on_stop graceful|ignore|exit <code>`; `stall` (blocks until a signal
//! ends it); `spin`; and the endings `ok [version]`, `fail <kind>`,
//! `exit <code>`, `crash` (abort), `kill` (SIGKILL itself).

use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

const GRACEFUL: i32 = 0;
const IGNORE: i32 = 1;
const EXIT: i32 = 2;

static ON_STOP: AtomicI32 = AtomicI32::new(GRACEFUL);
static STOP_CODE: AtomicI32 = AtomicI32::new(0);
static ACK_FD: AtomicI32 = AtomicI32::new(-1);
/// Built before the handler is installed, so the handler only writes.
static STOP_ACK: OnceLock<Vec<u8>> = OnceLock::new();

const CANCELLED: &[u8] = b"{\"event\":\"outcome\",\"failure\":\"cancelled\"}\n";

/// Only async-signal-safe calls: `write` and `_exit`.
extern "C" fn on_sigint(_: libc::c_int) {
    let ack = STOP_ACK.get().expect("set before the handler");
    let fd = ACK_FD.load(Ordering::SeqCst);
    // SAFETY: writes of buffers that live for the process.
    unsafe {
        libc::write(fd, ack.as_ptr().cast(), ack.len());
        match ON_STOP.load(Ordering::SeqCst) {
            GRACEFUL => {
                libc::write(1, CANCELLED.as_ptr().cast(), CANCELLED.len());
                libc::_exit(130);
            }
            EXIT => libc::_exit(STOP_CODE.load(Ordering::SeqCst)),
            _ => {}
        }
    }
}

fn open(path: &str, flags: libc::c_int) -> i32 {
    let c = CString::new(path).unwrap();
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    assert!(fd >= 0, "open {path}: {}", std::io::Error::last_os_error());
    fd
}

/// One line from `fd`, a byte at a time; `None` at end of file.
fn read_line(fd: i32) -> Option<String> {
    let mut line = Vec::new();
    loop {
        let mut b = 0u8;
        // SAFETY: one byte into a local.
        let n = unsafe { libc::read(fd, (&mut b as *mut u8).cast(), 1) };
        match n {
            1 if b == b'\n' => return Some(String::from_utf8_lossy(&line).into_owned()),
            1 => line.push(b),
            0 => return None,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => panic!("read: {}", std::io::Error::last_os_error()),
        }
    }
}

struct Puppet {
    step: String,
    pid: u32,
    ack_fd: i32,
    tree: std::path::PathBuf,
}

impl Puppet {
    fn ack(&self, what: &str) {
        let line = format!("{} {} {}\n", self.step, self.pid, what.replace('\n', " "));
        // SAFETY: a write of a live buffer; under PIPE_BUF, so atomic.
        unsafe { libc::write(self.ack_fd, line.as_ptr().cast(), line.len()) };
    }

    fn say(&self, event: serde_json::Value) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{event}").unwrap();
        out.flush().unwrap();
    }

    fn raw(&self, line: &str) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").unwrap();
        out.flush().unwrap();
    }

    /// Does `line`; returns only if the process goes on.
    fn obey(&self, line: &str) {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let args: Vec<&str> = rest.split_whitespace().collect();
        match verb {
            "write" => {
                std::fs::create_dir_all(&self.tree).unwrap();
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.tree.join("out.txt"))
                    .unwrap();
                writeln!(f, "{rest}").unwrap();
            }
            "fill" => {
                std::fs::create_dir_all(&self.tree).unwrap();
                let n: usize = args[0].parse().unwrap();
                std::fs::write(self.tree.join("fill.bin"), vec![b'x'; n]).unwrap();
            }
            "seal" => {
                let mut e =
                    serde_json::json!({"event": "checkpoint", "step": "me", "version": args[0]});
                if let Some(rows) = args.get(1) {
                    e["rows"] = serde_json::json!(rows.parse::<u64>().unwrap());
                }
                self.say(e);
            }
            "streams" => self.say(
                serde_json::json!({"event": "capabilities", "step": "me", "streams_output": true}),
            ),
            "metric" => self.say(serde_json::json!({
                "event": "metric", "step": "me", "name": args[0],
                "value": args[1].parse::<i64>().unwrap(),
            })),
            "progress_len" => self.say(serde_json::json!({
                "event": "progress_length", "step": "me",
                "total": args[0].parse::<u64>().unwrap(),
            })),
            "progress_inc" => self.say(serde_json::json!({
                "event": "progress_inc", "step": "me",
                "delta": args[0].parse::<u64>().unwrap(),
            })),
            "progress_msg" => self
                .say(serde_json::json!({"event": "progress_message", "step": "me", "msg": rest})),
            "log" => {
                let (level, msg) = rest.split_once(' ').unwrap_or((rest, ""));
                self.say(
                    serde_json::json!({"event": "log", "step": "me", "level": level, "msg": msg}),
                )
            }
            "raw" => self.raw(rest),
            "reads" => {
                let reads = std::env::var("DATALIB_READS").unwrap_or_default();
                return self.ack(&format!("reads {reads}"));
            }
            "on_stop" => {
                let mode = match args[0] {
                    "graceful" => GRACEFUL,
                    "ignore" => IGNORE,
                    "exit" => {
                        STOP_CODE.store(args[1].parse().unwrap(), Ordering::SeqCst);
                        EXIT
                    }
                    other => panic!("on_stop {other}"),
                };
                ON_STOP.store(mode, Ordering::SeqCst);
            }
            "stall" => {
                self.ack("did stall");
                loop {
                    // SAFETY: waits for a signal; the handler ends the
                    // process unless it is ignoring stops.
                    unsafe { libc::pause() };
                }
            }
            "spin" => {
                self.ack("did spin");
                loop {
                    std::hint::spin_loop();
                }
            }
            "ok" => {
                if let Some(v) = args.first() {
                    self.say(serde_json::json!({
                        "event": "outcome",
                        "outputs": [{"path": self.step, "version": v}],
                    }));
                }
                self.ack(&format!("did {line}"));
                std::process::exit(0);
            }
            "fail" => {
                self.say(serde_json::json!({"event": "outcome", "failure": args[0]}));
                self.ack(&format!("did fail {}", args[0]));
                std::process::exit(1);
            }
            "exit" => {
                self.ack(&format!("did exit {}", args[0]));
                std::process::exit(args[0].parse().unwrap());
            }
            "crash" => {
                self.ack("did crash");
                std::process::abort();
            }
            "kill" => {
                self.ack("did kill");
                // SAFETY: signals this process.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("SIGKILL returned");
            }
            other => panic!("no instruction {other:?}"),
        }
        self.ack(&format!("did {line}"));
    }
}

fn main() {
    let dir = std::env::var("PUPPET_DIR").expect("PUPPET_DIR");
    let step = std::env::var("DATALIB_DAG_STEP").expect("DATALIB_DAG_STEP");
    let root = std::env::var("DATALIB_DAG_DATA_ROOT").expect("DATALIB_DAG_DATA_ROOT");
    let attempt = std::env::var("DATALIB_DAG_ATTEMPT").unwrap_or_default();
    let puppet = Puppet {
        pid: std::process::id(),
        ack_fd: open(&format!("{dir}/acks"), libc::O_WRONLY),
        tree: std::path::Path::new(&root).join(&step),
        step,
    };
    ACK_FD.store(puppet.ack_fd, Ordering::SeqCst);
    let _ = STOP_ACK.set(format!("{} {} sigint\n", puppet.step, puppet.pid).into_bytes());
    // SAFETY: installs a handler that only writes and exits. No
    // SA_RESTART, so a read blocked on the next instruction is
    // interrupted and resumed by `read_line`.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    let fifo = format!("{dir}/{}.in", puppet.step.replace('/', "__"));
    let instructions = open(&fifo, libc::O_RDONLY);
    puppet.ack(&format!("started {attempt}"));
    while let Some(line) = read_line(instructions) {
        puppet.obey(&line);
    }
    panic!("the harness closed {fifo}");
}
