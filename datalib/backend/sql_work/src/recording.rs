//! Every statement run on the files under one folder, each with its work,
//! while a [`Recording`] of that folder lives: what a test compares when
//! it asks whether a statement's work grows with what else a store holds.
//! A folder, not a thread, because sqlx steps each connection on a thread
//! of its own, and tests in one binary each keep their stores in a folder
//! of their own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::Work;

/// One run of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The statement as written, its parameters unexpanded.
    pub sql: String,
    pub file: String,
    pub work: Work,
}

type Sink = Arc<Mutex<Vec<Ran>>>;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static RECORDINGS: Mutex<Vec<(Vec<PathBuf>, Sink)>> = Mutex::new(Vec::new());

/// Records until dropped. [`Recording::take`] hands over what it holds.
pub struct Recording {
    sink: Sink,
}

/// Start recording every statement run on a file under `dir`, on any
/// connection opened from now on.
pub fn record_under(dir: &Path) -> Recording {
    crate::install();
    let mut dirs = vec![dir.to_path_buf()];
    // A temporary folder on a mac is reached through a link
    // (`/var` → `/private/var`), and SQLite may name a file either way.
    if let Ok(real) = dir.canonicalize() {
        if real != dir {
            dirs.push(real);
        }
    }
    let sink = Sink::default();
    lock(&RECORDINGS).push((dirs, sink.clone()));
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    Recording { sink }
}

impl Recording {
    /// What was recorded since the start or the last take.
    pub fn take(&self) -> Vec<Ran> {
        std::mem::take(&mut *lock(&self.sink))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        lock(&RECORDINGS).retain(|(_, sink)| !Arc::ptr_eq(sink, &self.sink));
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn any() -> bool {
    ACTIVE.load(Ordering::Relaxed) > 0
}

pub(crate) fn note(sql: &str, file: &str, work: Work) {
    let file_path = Path::new(file);
    for (dirs, sink) in lock(&RECORDINGS).iter() {
        if dirs.iter().any(|d| file_path.starts_with(d)) {
            lock(sink).push(Ran {
                sql: sql.to_string(),
                file: file.to_string(),
                work,
            });
        }
    }
}

/// A recording a panicking test left poisoned still records.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
