//! A long-lived `serve` run of our SDK script that embeds search queries,
//! so the model loads once per process rather than once per search. qmd
//! does no retrieval here: the caller scores the index's vectors itself.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::{Qmd, SdkScript};

/// A query as the embedding model sees it, and which model that was: only
/// vectors the same model wrote are comparable with it.
#[derive(Debug, Clone)]
pub struct QueryEmbedding {
    pub model: String,
    pub vector: Vec<f32>,
}

pub struct QueryEmbedder {
    /// `None` resolves the pinned runtime at the first query, so a root
    /// that never searches by meaning never needs one.
    qmd: Option<Qmd>,
    root: PathBuf,
    deadline: Duration,
    live: Mutex<Option<Live>>,
}

struct Live {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    model: String,
    next_id: u64,
    // Deleted on drop, so it lives exactly as long as the process using it.
    _script: SdkScript,
}

impl QueryEmbedder {
    /// Spawns nothing until the first query.
    pub fn new(root: &Path, qmd: Option<Qmd>, deadline: Duration) -> Self {
        Self {
            qmd,
            root: root.to_path_buf(),
            deadline,
            live: Mutex::new(None),
        }
    }

    /// On any error, a missed deadline included, the process is stopped so
    /// the next query starts a fresh one.
    pub fn embed(&self, text: &str) -> Result<QueryEmbedding> {
        let mut guard = self
            .live
            .lock()
            .map_err(|_| anyhow!("query embedder mutex poisoned"))?;
        let deadline = Instant::now() + self.deadline;
        let res = (|| {
            if guard.is_none() {
                *guard = Some(self.spawn(deadline)?);
            }
            ask(guard.as_mut().expect("just started"), text, deadline)
        })();
        if res.is_err() {
            if let Some(mut live) = guard.take() {
                let _ = live.child.kill();
                let _ = live.child.wait();
            }
        }
        res
    }

    fn spawn(&self, deadline: Instant) -> Result<Live> {
        let qmd = match &self.qmd {
            Some(qmd) => qmd.clone(),
            None => Qmd::pinned()?,
        };
        let script = SdkScript::write()?;
        let cache_home = datalib_runtime::qmd::qmd_cache_home(&self.root);
        let mut child = std::process::Command::new(&qmd.node)
            // No node flags before the script: see the script's header.
            .arg(&script.0)
            .arg(&qmd.package)
            .arg(datalib_runtime::qmd::qmd_index_path(&self.root))
            .arg("serve")
            .arg("{}")
            .env("XDG_CACHE_HOME", &cache_home)
            .env("XDG_CONFIG_HOME", &cache_home)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn node for the query embedder; is the runtime staged?")?;
        let stdin = child.stdin.take().context("query embedder: no stdin")?;
        let stdout = child.stdout.take().context("query embedder: no stdout")?;
        // On a thread of its own, so waiting for an answer can give up.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut live = Live {
            child,
            stdin,
            lines,
            model: String::new(),
            next_id: 0,
            _script: script,
        };
        let ready = next_event(&live.lines, deadline)?;
        match ready["event"].as_str() {
            Some("ready") => {}
            Some("error") => bail!("query embedder: {}", ready["message"]),
            _ => bail!("query embedder: expected `ready`, got {ready}"),
        }
        live.model = ready["model"]
            .as_str()
            .context("query embedder: `ready` names no model")?
            .to_string();
        Ok(live)
    }
}

impl Drop for QueryEmbedder {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.live.lock() {
            if let Some(mut live) = guard.take() {
                let _ = live.child.kill();
                let _ = live.child.wait();
            }
        }
    }
}

fn ask(live: &mut Live, text: &str, deadline: Instant) -> Result<QueryEmbedding> {
    live.next_id += 1;
    let id = live.next_id;
    writeln!(live.stdin, "{}", json!({ "id": id, "text": text }))
        .and_then(|()| live.stdin.flush())
        .context("write to the query embedder")?;
    loop {
        let event = next_event(&live.lines, deadline)?;
        if event["id"].as_u64() != Some(id) {
            continue; // an answer to a request that already gave up
        }
        match event["event"].as_str() {
            Some("embedding") => {
                let model = event["model"].as_str().unwrap_or_default();
                if model != live.model {
                    bail!("query embedder answered with {model}, not {}", live.model);
                }
                let vector = event["vector"]
                    .as_array()
                    .context("query embedder: no vector")?
                    .iter()
                    .map(|x| x.as_f64().map(|x| x as f32))
                    .collect::<Option<Vec<f32>>>()
                    .context("query embedder: a vector entry is not a number")?;
                return Ok(QueryEmbedding {
                    model: live.model.clone(),
                    vector,
                });
            }
            Some("error") => bail!("query embedder: {}", event["message"]),
            _ => bail!("query embedder: unexpected answer {event}"),
        }
    }
}

/// The next JSON line; a line that is not JSON is a dependency writing to
/// stdout, and is skipped.
fn next_event(lines: &Receiver<String>, deadline: Instant) -> Result<Value> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match lines.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                bail!("the query embedder did not answer in time; it was stopped")
            }
            Err(RecvTimeoutError::Disconnected) => bail!("the query embedder exited"),
        };
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            return Ok(v);
        }
    }
}
