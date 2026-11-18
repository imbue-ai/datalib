//! The launch URL carries the API token, so it reaches only a terminal or
//! the parent that asked for it. Whatever keeps a server's stderr (the
//! desktop shell, a supervisor, a redirect to a file) is readable by any
//! process of the same user, including one a protected library's folder
//! keeps out, and with the token that process needs no folder at all.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(30);

fn server(root: &Path) -> Command {
    let bin = std::env::var_os("DATALIB_HTTP_BIN").expect("DATALIB_HTTP_BIN");
    let mut cmd = Command::new(bin);
    cmd.arg("--no-open")
        .arg(root)
        .env("DATALIB_BIND", "127.0.0.1:0")
        .env_remove("DATALIB_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn wait_for_token_file(root: &Path) -> String {
    let path = root.join("system/api-token");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Ok(token) = std::fs::read_to_string(&path) {
            if !token.trim().is_empty() {
                return token.trim().to_string();
            }
        }
        assert!(Instant::now() < deadline, "no {}", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Everything the server wrote to stderr, up to its death. The token file
/// is written after the moment the URL was once printed, so a server that
/// has written it has already said whatever it was going to.
fn stderr_of(mut child: Child) -> String {
    child.kill().expect("kill");
    let mut text = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut text)
        .expect("read stderr");
    child.wait().expect("wait");
    assert!(
        text.contains("datalib-http listening on"),
        "stderr was not captured: {text}"
    );
    text
}

/// The desktop shell's handshake: the URL arrives on the pipe it asked for,
/// and its capture of stderr, which is what it logs, does not hold the token.
#[test]
fn a_parent_gets_the_url_on_its_pipe_and_stderr_never_holds_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    let mut child = server(&root).arg("--url-stdout").spawn().expect("spawn");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(PATIENCE)
        .expect("the server announced no URL on stdout");
    let (origin, in_url) = line
        .trim()
        .split_once("/?token=")
        .unwrap_or_else(|| panic!("not a launch URL: {line:?}"));
    assert!(origin.starts_with("http://127.0.0.1:"), "{origin}");

    let token = wait_for_token_file(&root);
    assert_eq!(in_url, token, "the announced URL carries the real token");
    let stderr = stderr_of(child);
    assert!(!stderr.contains(&token), "token on stderr:\n{stderr}");
}

/// How Imbue Studio starts it: its launcher pins the token with
/// `DATALIB_TOKEN`, reads it back from the token file, and a supervisor
/// keeps stderr. The pin still decides the token, and the log stays clean.
#[test]
fn a_pinned_token_is_used_and_stays_off_a_supervised_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    let pinned = "a-token-the-parent-chose";
    let child = server(&root)
        .env("DATALIB_TOKEN", pinned)
        .spawn()
        .expect("spawn");

    assert_eq!(wait_for_token_file(&root), pinned);
    let stderr = stderr_of(child);
    assert!(!stderr.contains(pinned), "token on stderr:\n{stderr}");
}
