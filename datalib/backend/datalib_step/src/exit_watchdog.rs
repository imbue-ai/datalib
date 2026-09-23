//! A step that has printed its outcome has nothing left to do, but its
//! exit still runs the runtime's teardown, and on CI that teardown has
//! stalled until the caller's deadline with nothing said about where
//! (`render_contract_test`, Sep 2026). The watchdog makes a stalled exit
//! describe its threads, then ends it: the outcome already stands.

use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// `libc` is named only on Linux, for `prctl`.
#[cfg(not(target_os = "linux"))]
use libc as _;

/// The first words of the report, which `render_contract_test` looks for.
pub const MARKER: &str = "exit stalled after the outcome";

pub const GRACE: Duration = Duration::from_secs(30);

const GDB_DEADLINE: Duration = Duration::from_secs(30);

pub fn arm(grace: Duration) {
    std::thread::Builder::new()
        .name("exit-watchdog".into())
        .spawn(move || {
            std::thread::sleep(grace);
            let report = format!(
                "{MARKER}: still running {}s after it\n{}{}",
                grace.as_secs(),
                thread_states(Path::new("/proc/self/task")),
                gdb_backtraces(),
            );
            // Straight to fd 2, around std's stderr lock and the tracing
            // layer: either could be what the teardown is stuck in.
            // SAFETY: fd 2 stays open for the life of the process, and
            // ManuallyDrop keeps this handle from closing it.
            let mut stderr = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(2) });
            let _ = stderr.write_all(report.as_bytes());
            std::process::exit(0);
        })
        .expect("spawn the exit watchdog");
}

/// One line per thread: its name, the kernel function it sleeps in and
/// the syscall it is inside. Empty where there is no /proc (macOS).
fn thread_states(tasks: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(tasks) else {
        return String::new();
    };
    let read = |dir: &Path, file: &str| {
        std::fs::read_to_string(dir.join(file))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| format!("<{e}>"))
    };
    let mut out = String::from("threads (tid name wchan syscall):\n");
    for entry in entries.flatten() {
        let dir = entry.path();
        // A thread that exits between the listing and this read has left
        // nothing to report; every other thread keeps running meanwhile.
        let Ok(comm) = std::fs::read_to_string(dir.join("comm")) else {
            continue;
        };
        out += &format!(
            "  {} {} wchan={} syscall={}\n",
            entry.file_name().to_string_lossy(),
            comm.trim(),
            read(&dir, "wchan"),
            read(&dir, "syscall"),
        );
        // Kernel stack: readable only as root, which CI's container is.
        if let Ok(stack) = std::fs::read_to_string(dir.join("stack")) {
            for frame in stack.lines() {
                out += &format!("      {frame}\n");
            }
        }
    }
    out
}

/// Every thread's stack from `gdb`, when there is one on PATH.
fn gdb_backtraces() -> String {
    // Yama (ptrace_scope=1) lets a process be traced only by its
    // ancestors, and gdb is our child: say it may trace us.
    #[cfg(target_os = "linux")]
    // SAFETY: prctl with integer arguments touches no memory.
    unsafe {
        libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
    }
    let spawned = Command::new("gdb")
        .args(["-batch", "-nx", "-p", &std::process::id().to_string()])
        .args(["-ex", "thread apply all bt"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut gdb) = spawned else {
        return String::new();
    };
    let started = Instant::now();
    while matches!(gdb.try_wait(), Ok(None)) {
        if started.elapsed() > GDB_DEADLINE {
            let _ = gdb.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match gdb.wait_with_output() {
        Ok(out) => format!(
            "gdb:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => format!("gdb: {e}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report names the calling thread on Linux, and is empty rather
    /// than an error where /proc does not exist. Other tests' threads come
    /// and go while it reads, which once failed an assertion that no
    /// thread was unreadable.
    #[test]
    fn thread_states_names_this_thread_or_is_empty_without_proc() {
        let states = thread_states(Path::new("/proc/self/task"));
        #[cfg(target_os = "linux")]
        {
            // SAFETY: gettid takes no arguments and touches no memory.
            let tid = unsafe { libc::gettid() };
            let line = states
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{tid} ")))
                .unwrap_or_else(|| panic!("no line for thread {tid}:\n{states}"));
            assert!(
                line.contains(" wchan=") && !line.contains("wchan=<"),
                "{line}"
            );
        }
        #[cfg(not(target_os = "linux"))]
        assert_eq!(states, "");
        assert_eq!(thread_states(Path::new("/nonexistent/task")), "");
    }
}
