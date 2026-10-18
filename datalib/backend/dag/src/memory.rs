//! What a running subprocess step holds in memory: the resident memory of
//! the step and every process below it, sampled on an interval and
//! reported as two of the step's metrics. It measures; nothing here limits
//! a step.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Once};
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::events::{Event, EventSink};
use crate::step::StepId;

/// The resident bytes the step and everything it spawned hold now. Set to
/// zero when the step ends, so a finished step does not keep its last
/// reading as a current one.
pub const RSS_BYTES: &str = "rss_bytes";

/// The most [`RSS_BYTES`] has read during this attempt.
pub const RSS_PEAK_BYTES: &str = "rss_peak_bytes";

/// Shorter than the run store's five-second floor between samples, so the
/// peak, which the store does not coalesce away, catches a spike between
/// two stored samples.
const SAMPLE_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct Proc {
    parent: Option<u32>,
    rss: u64,
}

/// The resident bytes of `root` and of every process whose chain of
/// parents reaches it. A process whose parent exits is handed to init and
/// drops out of the sum, so it is a floor, not an exact total.
fn tree_rss(procs: &HashMap<u32, Proc>, root: u32) -> u64 {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, p) in procs {
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut total = 0;
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        // A snapshot taken while pids are reused can, in principle, link a
        // process to its own descendant.
        if !seen.insert(pid) {
            continue;
        }
        let Some(p) = procs.get(&pid) else { continue };
        total += p.rss;
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    total
}

struct Sampler {
    sys: System,
    root: u32,
}

impl Sampler {
    fn new(root: u32) -> Self {
        // On Linux sysinfo otherwise keeps a `/proc` file open for every
        // process it has read, up to half the descriptor limit, and the
        // runner shares that limit with every store it opens.
        static NO_HELD_FILES: Once = Once::new();
        NO_HELD_FILES.call_once(|| {
            sysinfo::set_open_files_limit(0);
        });
        Self {
            sys: System::new(),
            root,
        }
    }

    fn sample(&mut self) -> u64 {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            // Without tasks, or Linux lists each thread as a process of its
            // own and the sum counts its memory once per thread.
            ProcessRefreshKind::nothing().with_memory().without_tasks(),
        );
        let procs = self
            .sys
            .processes()
            .iter()
            .filter(|(_, p)| p.thread_kind().is_none())
            .map(|(pid, p)| {
                let proc = Proc {
                    parent: p.parent().map(|parent| parent.as_u32()),
                    rss: p.memory(),
                };
                (pid.as_u32(), proc)
            })
            .collect();
        tree_rss(&procs, self.root)
    }
}

/// Reports a step's memory while it runs. Dropping it stops the sampling
/// and reports [`RSS_BYTES`] as zero, on every path out of the step.
pub(crate) struct MemoryWatch {
    task: tokio::task::JoinHandle<()>,
    step: StepId,
    sink: Arc<dyn EventSink>,
}

impl MemoryWatch {
    pub(crate) fn start(pid: u32, step: &str, sink: &Arc<dyn EventSink>) -> Self {
        let task = {
            let step = step.to_string();
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut sampler = Sampler::new(pid);
                let mut peak = 0;
                let mut tick = tokio::time::interval(SAMPLE_EVERY);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    // Reading every process's memory is a few milliseconds of
                    // system calls, too long to hold a runtime worker for.
                    let sampled = tokio::task::spawn_blocking(move || {
                        let rss = sampler.sample();
                        (sampler, rss)
                    })
                    .await;
                    let Ok((back, rss)) = sampled else { return };
                    sampler = back;
                    emit(&sink, &step, RSS_BYTES, rss);
                    if rss > peak {
                        peak = rss;
                        emit(&sink, &step, RSS_PEAK_BYTES, peak);
                    }
                }
            })
        };
        Self {
            task,
            step: step.to_string(),
            sink: sink.clone(),
        }
    }
}

impl Drop for MemoryWatch {
    fn drop(&mut self) {
        // A sample still being read finishes on its blocking thread, but
        // the aborted task never emits it, so the zero below is the last word.
        self.task.abort();
        emit(&self.sink, &self.step, RSS_BYTES, 0);
    }
}

fn emit(sink: &Arc<dyn EventSink>, step: &str, name: &str, bytes: u64) {
    sink.emit(&Event::Metric {
        step: step.to_string(),
        name: name.to_string(),
        labels: Default::default(),
        value: i64::try_from(bytes).unwrap_or(i64::MAX),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs(rows: &[(u32, Option<u32>, u64)]) -> HashMap<u32, Proc> {
        rows.iter()
            .map(|&(pid, parent, rss)| (pid, Proc { parent, rss }))
            .collect()
    }

    /// The step's own memory is the small part when it is a wrapper: the
    /// `embed` step's node child holds the model. Counting only the pid
    /// would report a shell's megabyte.
    #[test]
    fn a_step_counts_its_children_and_theirs_and_nothing_beside_it() {
        let table = procs(&[
            (1, None, 1_000),
            (10, Some(1), 5),     // the runner
            (20, Some(10), 7),    // the step
            (21, Some(20), 100),  // its child
            (22, Some(21), 1000), // its grandchild
            (30, Some(10), 9999), // another step
        ]);
        assert_eq!(tree_rss(&table, 20), 1107);
    }

    #[test]
    fn a_step_that_has_exited_holds_nothing() {
        let table = procs(&[(1, None, 1_000), (21, Some(20), 100)]);
        assert_eq!(tree_rss(&table, 20), 0);
    }

    /// A parent loop must not hang the sampler.
    #[test]
    fn a_loop_of_parents_is_counted_once_and_ends() {
        let table = procs(&[(20, Some(21), 7), (21, Some(20), 100)]);
        assert_eq!(tree_rss(&table, 20), 107);
    }
}
