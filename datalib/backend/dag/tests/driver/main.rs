//! A step that does only what it is told, for the supervisor's scenario
//! tests (`tests/harness/`). It reads one instruction a line from the FIFO
//! `$DRIVER_CONTROL/<step>.in` and answers on `$DRIVER_CONTROL/<step>.acks`
//! only once the effect is durable: an acknowledged instruction happened,
//! and one never acknowledged left no trace. The instructions are the
//! arms of `Driver::run`.
//!
//! It never sleeps. It blocks on the FIFO, and `stall` blocks on a signal
//! that only a stop sends.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use datalib_etl::doltlite_raw;
use serde_json::json;

const STORE: &str = "store.doltlite_db";
const ROWS_DDL: &str = "CREATE TABLE IF NOT EXISTS rows (id TEXT PRIMARY KEY, n INTEGER NOT NULL)";

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
    root: PathBuf,
    tree: PathBuf,
    acks: File,
    rt: tokio::runtime::Runtime,
    store: Option<sqlx::SqlitePool>,
    on_stop: OnStop,
}

fn main() -> Result<()> {
    catch_sigint();
    let step = std::env::var("DATALIB_DAG_STEP").context("DATALIB_DAG_STEP")?;
    let root =
        PathBuf::from(std::env::var("DATALIB_DAG_DATA_ROOT").context("DATALIB_DAG_DATA_ROOT")?);
    let control = PathBuf::from(std::env::var("DRIVER_CONTROL").context("DRIVER_CONTROL")?);
    let key = step.replace('/', "__");
    // Both FIFOs are held open by the harness, so neither open waits.
    let mut input =
        File::open(control.join(format!("{key}.in"))).context("open the instructions")?;
    let acks = std::fs::OpenOptions::new()
        .write(true)
        .open(control.join(format!("{key}.acks")))
        .context("open the acks")?;
    let tree = root.join(&step);
    std::fs::create_dir_all(&tree)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut d = Driver {
        step,
        root,
        tree,
        acks,
        rt,
        store: None,
        on_stop: OnStop::Graceful,
    };
    let attempt = std::env::var("DATALIB_DAG_ATTEMPT").unwrap_or_default();
    d.ack(&format!("started {attempt}"))?;
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
            // Files in the step's own tree, each written whole or not at all.
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
                let mut ev = json!({"event": "checkpoint", "step": "", "version": args.first().context("seal <version> [rows]")?});
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
                self.say(json!({"event": "metric", "step": "", "name": name, "labels": labels, "value": value}))?;
            }
            "progress_length" => self.say(
                json!({"event": "progress_length", "step": "", "total": args[0].parse::<u64>()?}),
            )?,
            "progress_inc" => self.say(
                json!({"event": "progress_inc", "step": "", "delta": args[0].parse::<u64>()?}),
            )?,
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
            // The one doltlite table: `batch commit|hold put:<id>:<n> del:<id> …`.
            "batch" => {
                let commit = match args.first() {
                    Some(&"commit") => true,
                    Some(&"hold") => false,
                    _ => bail!("batch commit|hold <ops>…"),
                };
                self.apply(&args[1..])?;
                if commit {
                    let hash = self.commit()?;
                    return self.ack(&format!("committed {}", hash.as_deref().unwrap_or("-")));
                }
                return self.ack("held");
            }
            "commit" => {
                let hash = self.commit()?;
                return self.ack(&format!("committed {}", hash.as_deref().unwrap_or("-")));
            }
            // A consumer: how many rows each input's store has at the version
            // this invocation was started against.
            "count" => {
                let counts = self.count_inputs()?;
                return self.ack(&format!("count {counts}"));
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
            // Ways to end.
            "ok" => {
                let mut ev = json!({"event": "outcome"});
                if let Some(version) = args.first() {
                    ev["outputs"] = json!([{"path": self.step, "version": version}]);
                }
                self.say(ev)?;
                self.close_store();
                self.ack("exiting 0")?;
                std::process::exit(0);
            }
            "fail" => {
                let kind = args.first().context("fail <kind>")?;
                self.say(json!({"event": "outcome", "failure": kind}))?;
                self.close_store();
                self.ack(&format!("failing {kind}"))?;
                std::process::exit(1);
            }
            "exit" => {
                let code: i32 = args.first().context("exit <code>")?.parse()?;
                self.close_store();
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

    fn apply(&mut self, ops: &[&str]) -> Result<()> {
        let db = self.tree.join(STORE);
        let rt = &self.rt;
        if self.store.is_none() {
            self.store = Some(rt.block_on(doltlite_raw::open(&db, &[ROWS_DDL]))?);
        }
        let pool = self.store.as_ref().expect("opened above");
        rt.block_on(async {
            let mut tx = pool.begin().await?;
            for op in ops {
                match op.split(':').collect::<Vec<_>>().as_slice() {
                    ["put", id, n] => {
                        sqlx::query(
                            "INSERT INTO rows (id, n) VALUES (?, ?) \
                             ON CONFLICT(id) DO UPDATE SET n = excluded.n",
                        )
                        .bind(*id)
                        .bind(n.parse::<i64>()?)
                        .execute(&mut *tx)
                        .await?;
                    }
                    ["del", id] => {
                        sqlx::query("DELETE FROM rows WHERE id = ?")
                            .bind(*id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    _ => bail!("an op is put:<id>:<n> or del:<id>, not {op:?}"),
                }
            }
            tx.commit().await?;
            Ok(())
        })
    }

    /// `None` when nothing was committed, which includes this process
    /// never having opened the store.
    fn commit(&mut self) -> Result<Option<String>> {
        let Some(pool) = self.store.as_ref() else {
            return Ok(None);
        };
        self.rt.block_on(doltlite_raw::commit_run(pool, "driver"))
    }

    fn close_store(&mut self) {
        if let Some(pool) = self.store.take() {
            self.rt.block_on(pool.close());
        }
    }

    /// `path=count` for each input, space-separated, in input order.
    fn count_inputs(&self) -> Result<String> {
        let reads: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&std::env::var("DATALIB_READS").unwrap_or_else(|_| "{}".into()))?;
        let inputs = std::env::var("DATALIB_DAG_INPUTS").unwrap_or_default();
        let mut out = Vec::new();
        for input in inputs.lines().filter(|l| !l.is_empty()) {
            let commit = reads
                .get(input)
                .and_then(|v| v.as_str())
                .and_then(|v| store_head(v))
                .map(str::to_string);
            let n = self.rt.block_on(count_at(
                &self.root.join(input).join(STORE),
                commit.as_deref(),
            ))?;
            out.push(format!("{input}={n}"));
        }
        Ok(out.join(" "))
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
                self.close_store();
                self.ack("stopped")?;
                std::process::exit(130);
            }
            OnStop::Exit(code) => {
                self.close_store();
                self.ack(&format!("exiting {code}"))?;
                std::process::exit(code);
            }
        }
    }
}

/// The commit a sink version names for our store: `store.doltlite_db:<hash>`
/// among the tree's stores.
fn store_head(version: &str) -> Option<&str> {
    version
        .split(' ')
        .find_map(|part| part.strip_prefix(&format!("{STORE}:")))
        .filter(|h| *h != "-")
}

async fn count_at(db: &Path, commit: Option<&str>) -> Result<i64> {
    if !db.exists() {
        return Ok(0);
    }
    let Some(reader) = doltlite_raw::open_reader(db, commit).await? else {
        return Ok(0);
    };
    let n = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pinned_rows")
        .fetch_one(reader.pool())
        .await;
    reader.close().await;
    Ok(n?)
}
