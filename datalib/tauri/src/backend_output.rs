//! The desktop shell's end of its backend's stdout and stderr: the launch
//! URL arrives over the pipe, and the rest of the output goes in the
//! library. README.md, "A protected library", says why nothing may go
//! anywhere else: the URL carries the API token, and a file's mode does not
//! keep out another process of the same user.
//!
//! Nothing here may reference `tauri`: `//datalib/tauri:backend_output_test`
//! compiles this file on its own.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TAIL_LINES: usize = 20;
const POLL: Duration = Duration::from_millis(50);
/// How long a dead server's last words get to come out of the pipe. A
/// program it started can hold the pipe open past its death.
const LAST_WORDS: Duration = Duration::from_secs(1);

pub fn output_file(root: &Path) -> PathBuf {
    root.join("system").join("server-output.log")
}

fn previous_output_file(root: &Path) -> PathBuf {
    root.join("system").join("server-output.previous.log")
}

/// The shell has no progress bars for this to collide with.
#[allow(clippy::disallowed_macros)]
fn report(message: &str) {
    eprintln!("{message}");
}

#[derive(Default)]
struct Output {
    tail: VecDeque<String>,
    file: Option<File>,
}

impl Output {
    fn add(&mut self, line: String) {
        if let Some(file) = &mut self.file {
            let _ = writeln!(file, "{line}");
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    fn keep_in(&mut self, mut file: File) {
        for line in &self.tail {
            let _ = writeln!(file, "{line}");
        }
        self.file = Some(file);
    }

    fn quote_tail(&self) -> String {
        if self.tail.is_empty() {
            return "(the server wrote nothing)".into();
        }
        Vec::from_iter(self.tail.iter().map(String::as_str)).join("\n")
    }
}

/// Lossy, so a stray byte cannot stop the reading and leave the writer
/// blocked on a full pipe.
fn keep_lines(mut pipe: impl BufRead, output: &Mutex<Output>) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match pipe.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line).trim_end().to_string();
                output.lock().expect("output lock").add(text);
            }
        }
    }
}

fn read_stderr(pipe: impl Read + Send + 'static, output: Arc<Mutex<Output>>) -> JoinHandle<()> {
    std::thread::spawn(move || keep_lines(BufReader::new(pipe), &output))
}

/// The first line is the URL, and goes nowhere but the channel.
fn read_stdout(
    pipe: impl Read + Send + 'static,
    output: Arc<Mutex<Output>>,
) -> mpsc::Receiver<String> {
    let (announce, announced) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pipe = BufReader::new(pipe);
        let mut first = String::new();
        if pipe.read_line(&mut first).is_ok() {
            let _ = announce.send(first.trim().to_string());
        }
        keep_lines(pipe, &output);
    });
    announced
}

fn wait_for_last_words(stderr: &JoinHandle<()>) {
    let deadline = Instant::now() + LAST_WORDS;
    while !stderr.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

/// A server that announces has taken the root's lock, so it is the only one
/// using these files. The previous run's stays for one more run: a person
/// who reopens the app after a crash still has what the crash said.
fn open_output_file(root: &Path) -> std::io::Result<File> {
    let current = output_file(root);
    match std::fs::rename(&current, previous_output_file(root)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    File::create(current)
}

/// Wait for a `datalib-http --url-stdout` child, whose stdout and stderr are
/// pipes, to announce its URL; its output keeps being read for as long as it
/// runs. `Err` quotes its last lines, since a Finder-launched app has no
/// terminal, and a child that outlasts `patience` is killed first.
pub fn await_url(child: &mut Child, root: &Path, patience: Duration) -> Result<String, String> {
    let output = Arc::new(Mutex::new(Output::default()));
    let stdout = child
        .stdout
        .take()
        .ok_or("the server's stdout is no pipe")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("the server's stderr is no pipe")?;
    let announced = read_stdout(stdout, output.clone());
    let stderr = read_stderr(stderr, output.clone());
    let quote_tail = || output.lock().expect("output lock").quote_tail();

    let deadline = Instant::now() + patience;
    loop {
        if let Ok(url) = announced.try_recv() {
            if !url.is_empty() {
                match open_output_file(root) {
                    Ok(file) => output.lock().expect("output lock").keep_in(file),
                    Err(e) => report(&format!("could not keep the server's output: {e}")),
                }
                return Ok(url);
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            wait_for_last_words(&stderr);
            return Err(format!(
                "datalib-http exited during startup ({status}):\n{}",
                quote_tail()
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            wait_for_last_words(&stderr);
            return Err(format!(
                "datalib-http did not announce its URL within {}s:\n{}",
                patience.as_secs(),
                quote_tail()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// A file an earlier build kept beside the temp dir's other files, holding
/// the server's URL with its token: `datalib-http-<pid>.log` or `.url`.
fn is_left_behind(file_name: &str) -> bool {
    let pid = file_name
        .strip_prefix("datalib-http-")
        .and_then(|rest| rest.strip_suffix(".log").or(rest.strip_suffix(".url")));
    pid.is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

// TODO(after 2027-02-05): remove with `is_left_behind`. By then an upgrade
// has run this, or macOS has cleared the temp dir.
pub fn remove_left_behind(temp_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        if is_file && name.to_str().is_some_and(is_left_behind) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                report(&format!("could not remove {}: {e}", entry.path().display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const URL: &str = "http://127.0.0.1:1/?token=the-token";
    const PATIENCE: Duration = Duration::from_secs(30);

    fn sh(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh")
    }

    fn root_with_system() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("system")).unwrap();
        root
    }

    fn wait_for_output_line(root: &Path, line: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let kept = std::fs::read_to_string(output_file(root)).unwrap_or_default();
            if kept.lines().any(|l| l == line) {
                return kept;
            }
            assert!(Instant::now() < deadline, "never kept {line:?}: {kept}");
            std::thread::sleep(POLL);
        }
    }

    /// The URL carries the API token, so the one place it must not reach is
    /// a file: the library's own log included, since that can be copied out
    /// of the protected folder. What the server says on either pipe, before
    /// or after the URL, is kept.
    #[test]
    fn the_url_comes_over_the_pipe_and_only_the_output_is_kept() {
        let root = root_with_system();
        let script =
            format!("echo before >&2; echo '{URL}'; echo on-stdout; echo after >&2; exec sleep 60");
        let mut child = sh(&script);

        let url = await_url(&mut child, root.path(), PATIENCE).unwrap();
        assert_eq!(url, URL);

        wait_for_output_line(root.path(), "before");
        wait_for_output_line(root.path(), "on-stdout");
        let kept = wait_for_output_line(root.path(), "after");
        assert!(!kept.contains("the-token"), "{kept}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_server_that_dies_says_why_with_its_last_lines() {
        let root = root_with_system();
        let mut child = sh("echo 'data root refused' >&2; exit 3");

        let refused = await_url(&mut child, root.path(), PATIENCE).unwrap_err();
        assert!(refused.contains("data root refused"), "{refused}");
        assert!(refused.contains("exit"), "{refused}");
    }

    #[test]
    fn a_server_that_never_announces_is_killed_and_quoted() {
        let root = root_with_system();
        let mut child = sh("echo stuck >&2; exec sleep 60");

        let refused = await_url(&mut child, root.path(), Duration::from_secs(1)).unwrap_err();
        assert!(refused.contains("did not announce"), "{refused}");
        assert!(refused.contains("stuck"), "{refused}");
        assert!(child.try_wait().unwrap().is_some(), "still running");
    }

    /// A long-running app must not grow without limit in memory, and the
    /// dialog quotes only the end.
    #[test]
    fn the_tail_keeps_the_last_lines() {
        let mut output = Output::default();
        for n in 0..=TAIL_LINES {
            output.add(format!("line {n}"));
        }
        let quoted = output.quote_tail();
        assert!(!quoted.contains("line 0\n"), "{quoted}");
        assert!(quoted.ends_with(&format!("line {TAIL_LINES}")), "{quoted}");
        assert_eq!(quoted.lines().count(), TAIL_LINES);
    }

    /// Reopening the app after a crash must not erase what the crash said.
    #[test]
    fn the_previous_runs_output_is_kept_once() {
        let root = root_with_system();
        let root = root.path();
        for run in ["first", "second", "third"] {
            let mut file = open_output_file(root).unwrap();
            writeln!(file, "{run}").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(output_file(root)).unwrap(),
            "third\n"
        );
        assert_eq!(
            std::fs::read_to_string(previous_output_file(root)).unwrap(),
            "second\n"
        );
    }

    /// What an earlier build left goes, and nothing else in a temp dir
    /// shared with every other program does.
    #[test]
    fn only_files_an_earlier_build_left_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let gone = ["datalib-http-123.log", "datalib-http-123.url"];
        let kept = [
            "datalib-http-.log",
            "datalib-http-abc.log",
            "datalib-http-123.log.bak",
            "datalib-http-123.txt",
            "xdatalib-http-123.log",
            "other.log",
        ];
        for name in gone.iter().chain(&kept) {
            std::fs::write(path(name), "x").unwrap();
        }
        std::fs::create_dir(path("datalib-http-9.log")).unwrap();

        remove_left_behind(dir.path());

        for name in gone {
            assert!(!path(name).exists(), "{name} should be gone");
        }
        for name in kept {
            assert!(path(name).exists(), "{name} should be kept");
        }
        assert!(
            path("datalib-http-9.log").is_dir(),
            "a folder is left alone"
        );
    }
}
