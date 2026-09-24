//! A puppet step for the supervisor's scenario tests (`tests/harness/`):
//! a process the loop manages that does only what it is told. It reads one
//! instruction a line from the FIFO `$DRIVER_CONTROL/<step>.in` and answers
//! on `$DRIVER_CONTROL/<step>.acks` once it has done it. The instructions
//! are the arms of `Driver::run`.
//!
//! It exercises the loop, not storage: what it writes is incidental, and the
//! versions the loop tracks are the ones it reports (`seal`, `ok`). Whether a
//! step's own writes are atomic is `etl`'s `doltlite_interrupt_test`.
//!
//! It never sleeps. It blocks on the FIFO, and `stall` blocks on a signal
//! that only a stop sends.

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use serde_json::json;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Without `SA_RESTART`, so a blocked read returns `Interrupted` and the
/// driver answers the stop instead of reading on.
fn catch_sigint() {
    // SAFETY: a zeroed sigaction with a handler that only stores an atomic.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigint as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

#[derive(Clone, Copy)]
enum OnStop {
    /// What a real step does: stop, say cancelled, exit 130.
    Graceful,
    /// Carry on as if nothing came, to be killed at the grace.
    Ignore,
    Exit(i32),
}

struct Driver {
    step: String,
    tree: PathBuf,
    acks: File,
    on_stop: OnStop,
}

fn main() -> Result<()> {
    catch_sigint();
    let step = std::env::var("DATALIB_DAG_STEP").context("DATALIB_DAG_STEP")?;
    let root = std::env::var("DATALIB_DAG_DATA_ROOT").context("DATALIB_DAG_DATA_ROOT")?;
    let control = PathBuf::from(std::env::var("DRIVER_CONTROL").context("DRIVER_CONTROL")?);
    let key = step.replace('/', "__");
    // Both FIFOs are held open by the harness, so neither open waits.
    let mut input =
        File::open(control.join(format!("{key}.in"))).context("open the instructions")?;
    let acks = std::fs::OpenOptions::new()
        .write(true)
        .open(control.join(format!("{key}.acks")))
        .context("open the acks")?;
    let tree = PathBuf::from(root).join(&step);
    std::fs::create_dir_all(&tree)?;
    let mut d = Driver {
        step,
        tree,
        acks,
        on_stop: OnStop::Graceful,
    };
    let attempt = std::env::var("DATALIB_DAG_ATTEMPT").unwrap_or_default();
    d.ack(&format!("started {attempt} pid={}", std::process::id()))?;
    loop {
        match read_line(&mut input)? {
            Some(line) => d.run(line.trim())?,
            None => d.stopped()?,
        }
    }
}

/// One line, a byte at a time: a buffered reader would take the next
/// invocation's instructions with it when this one dies. `None` when a
/// stop interrupted the read.
fn read_line(input: &mut File) -> Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if STOP.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match input.read(&mut byte) {
            Ok(0) => bail!("the harness closed the instructions"),
            Ok(_) if byte[0] == b'\n' => return Ok(Some(String::from_utf8(line)?)),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

impl Driver {
    /// Written in one `write`, under `PIPE_BUF`, so acks never interleave.
    fn ack(&mut self, what: &str) -> Result<()> {
        let line = format!("{} {what}\n", self.step);
        self.acks.write_all(line.as_bytes())?;
        Ok(())
    }

    fn say(&self, event: serde_json::Value) -> Result<()> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{event}")?;
        out.flush()?;
        Ok(())
    }

    fn run(&mut self, line: &str) -> Result<()> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let args: Vec<&str> = rest.split_whitespace().collect();
        match verb {
            // Incidental output: a file in the step's own tree, or a big one,
            // for the IO a real step makes.
            "write" => {
                let (rel, text) = rest.split_once(' ').unwrap_or((rest, ""));
                self.write_file(rel, text.as_bytes())?;
            }
            "fill" => {
                let bytes: usize = args.get(1).context("fill <rel> <bytes>")?.parse()?;
                self.write_file(args[0], &vec![b'x'; bytes])?;
            }
            // The stdout protocol, one line each.
            "streams" => {
                self.say(json!({"event": "capabilities", "step": "", "streams_output": true}))?
            }
            "seal" => {
                let version = args.first().context("seal <version> [rows]")?;
                let mut ev = json!({"event": "checkpoint", "step": "", "version": version});
                if let Some(rows) = args.get(1) {
                    ev["rows"] = json!(rows.parse::<u64>()?);
                }
                self.say(ev)?;
            }
            "metric" => {
                let name = args.first().context("metric <name> <value> [k=v]…")?;
                let value: i64 = args.get(1).context("metric value")?.parse()?;
                let labels: serde_json::Map<String, serde_json::Value> = args[2..]
                    .iter()
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect();
                self.say(json!({
                    "event": "metric", "step": "", "name": name, "labels": labels, "value": value,
                }))?;
            }
            "progress_length" => self.say(json!({
                "event": "progress_length", "step": "", "total": args[0].parse::<u64>()?,
            }))?,
            "progress_inc" => self.say(json!({
                "event": "progress_inc", "step": "", "delta": args[0].parse::<u64>()?,
            }))?,
            "progress_message" => {
                self.say(json!({"event": "progress_message", "step": "", "msg": rest}))?
            }
            "log" => {
                let (level, msg) = rest.split_once(' ').unwrap_or((rest, ""));
                self.say(json!({"event": "log", "step": "", "level": level, "msg": msg}))?;
            }
            "raw" => {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{rest}")?;
                out.flush()?;
            }
            // What the loop started this invocation against: each input's
            // version, as `path=version`, in input order.
            "reads" => {
                let reads = reads()?;
                return self.ack(&format!("reads {reads}"));
            }
            "on_stop" => {
                self.on_stop = match args.as_slice() {
                    ["graceful"] => OnStop::Graceful,
                    ["ignore"] => OnStop::Ignore,
                    ["exit", code] => OnStop::Exit(code.parse()?),
                    _ => bail!("on_stop graceful|ignore|exit <code>"),
                };
            }
            // Doing nothing until stopped, in two ways.
            "stall" => {
                self.ack("stalling")?;
                loop {
                    // SAFETY: waits for any signal; the handler only stores.
                    unsafe { libc::pause() };
                    if STOP.load(Ordering::SeqCst) {
                        self.stopped()?;
                    }
                }
            }
            "spin" => {
                self.ack("spinning")?;
                loop {
                    if STOP.load(Ordering::SeqCst) {
                        self.stopped()?;
                    }
                    std::hint::spin_loop();
                }
            }
            // Ways to end. `ok <version>` reports the tree's version, which
            // the loop then trusts verbatim.
            "ok" => {
                let mut ev = json!({"event": "outcome"});
                if let Some(version) = args.first() {
                    ev["outputs"] = json!([{"path": self.step, "version": version}]);
                }
                self.say(ev)?;
                self.ack("exiting 0")?;
                std::process::exit(0);
            }
            "fail" => {
                let kind = args.first().context("fail <kind>")?;
                self.say(json!({"event": "outcome", "failure": kind}))?;
                self.ack(&format!("failing {kind}"))?;
                std::process::exit(1);
            }
            "exit" => {
                let code: i32 = args.first().context("exit <code>")?.parse()?;
                self.ack(&format!("exiting {code}"))?;
                std::process::exit(code);
            }
            "crash" => {
                self.ack("crashing")?;
                std::process::abort();
            }
            "kill" => {
                self.ack("killing")?;
                // SAFETY: signalling our own process.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            }
            other => bail!("unknown instruction {other:?}"),
        }
        self.ack(line)
    }

    fn write_file(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.tree.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("driver-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Answer a stop the way `on_stop` says.
    fn stopped(&mut self) -> Result<()> {
        match self.on_stop {
            OnStop::Ignore => {
                STOP.store(false, Ordering::SeqCst);
                self.ack("ignoring a stop")
            }
            OnStop::Graceful => {
                self.say(json!({"event": "outcome", "failure": "cancelled"}))?;
                self.ack("stopped")?;
                std::process::exit(130);
            }
            OnStop::Exit(code) => {
                self.ack(&format!("exiting {code}"))?;
                std::process::exit(code);
            }
        }
    }
}

fn reads() -> Result<String> {
    let reads: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&std::env::var("DATALIB_READS").unwrap_or_else(|_| "{}".into()))?;
    let inputs = std::env::var("DATALIB_DAG_INPUTS").unwrap_or_default();
    Ok(inputs
        .lines()
        .filter(|l| !l.is_empty())
        .map(|input| {
            let v = reads.get(input).and_then(|v| v.as_str()).unwrap_or("-");
            format!("{input}={v}")
        })
        .collect::<Vec<_>>()
        .join(" "))
}
