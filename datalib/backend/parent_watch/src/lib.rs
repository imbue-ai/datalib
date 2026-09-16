//! Exit when the process that started us is gone — however it went.
//!
//! A parent that exits cleanly, or on a signal, can kill its children on
//! the way out. A parent that is SIGKILLed runs no code at all, and its
//! children live on with their ports and their data roots open until
//! somebody notices (#238 counted 186 of them). The one thing that does
//! survive a SIGKILL is the kernel closing the parent's file descriptors,
//! so the child holds the read end of a pipe the parent never writes to,
//! and treats EOF on it as "the parent is gone".
//!
//! Every long-running datalib process a datalib process starts uses this:
//! the gateway's applets, the desktop shell's `datalib-http`, the e2e
//! suite's backends. The spawner hands over stdin as a pipe (or a socket
//! pair — that is what Node's `stdio: "pipe"` actually makes) and sets
//! [`ENV_VAR`]; the child calls [`exit_with_parent`] first thing.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;

/// Set to any value by a spawner that has made stdin a parent pipe.
///
/// An opt-in rather than the default, because a program run by hand has a
/// terminal on stdin (reading it would swallow the user's input) or
/// `/dev/null` (an immediate EOF would look like a dead parent).
pub const ENV_VAR: &str = "DATALIB_PARENT_PIPE";

/// [`ENV_VAR`] was set, but stdin is not something a parent can hold open.
///
/// Refusing to start is deliberate. The alternative — carrying on with no
/// watch — is a protection that is silently absent, which is the failure
/// this crate exists to prevent.
#[derive(Debug)]
pub struct NotAPipe {
    found: String,
}

impl fmt::Display for NotAPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{ENV_VAR} is set but stdin is {}, not a pipe: the spawner asked \
             for a parent watch and did not provide the pipe it watches",
            self.found
        )
    }
}

impl std::error::Error for NotAPipe {}

/// Start watching stdin for EOF if the spawner asked for it; call
/// `on_parent_gone` from a background thread when it arrives.
///
/// Returns `Ok(())` without doing anything when [`ENV_VAR`] is unset.
/// The callback runs on a thread of its own, after the parent is already
/// dead, so it must not write to stderr with anything that panics on a
/// failed write — see [`report`].
pub fn exit_with_parent<F>(on_parent_gone: F) -> Result<(), NotAPipe>
where
    F: FnOnce() + Send + 'static,
{
    if std::env::var_os(ENV_VAR).is_none() {
        return Ok(());
    }
    let stdin = std::io::stdin();
    let kind = stdin
        .as_fd()
        .try_clone_to_owned()
        .map(File::from)
        .and_then(|f| f.metadata())
        .map(|m| m.file_type());
    match kind {
        Ok(t) if t.is_fifo() || t.is_socket() => {}
        Ok(t) => return Err(NotAPipe { found: describe(t) }),
        Err(e) => {
            return Err(NotAPipe {
                found: format!("unreadable ({e})"),
            })
        }
    }
    std::thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            let mut stdin = stdin.lock();
            let mut scratch = [0u8; 64];
            loop {
                match stdin.read(&mut scratch) {
                    Ok(0) => break,
                    // Nothing is supposed to arrive, but a byte is not a
                    // reason to die.
                    Ok(_) => continue,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            on_parent_gone();
        })
        .expect("spawn the parent-watch thread");
    Ok(())
}

/// Best-effort stderr, for use from `on_parent_gone`.
///
/// By the time the callback runs, stderr is usually a pipe to the process
/// that just died, so the write takes `EPIPE`. `eprintln!` *panics* on a
/// failed write, and a panic on the watch thread leaves the process
/// running — the leak being fixed, reintroduced one line from the exit
/// that fixes it.
pub fn report(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{msg}");
}

fn describe(t: std::fs::FileType) -> String {
    if t.is_char_device() {
        "a character device (a terminal, or /dev/null)".into()
    } else if t.is_file() {
        "a regular file".into()
    } else if t.is_dir() {
        "a directory".into()
    } else if t.is_block_device() {
        "a block device".into()
    } else {
        "of an unexpected type".into()
    }
}
