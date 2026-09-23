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
//! Every long-running datalib process a datalib process starts uses this.
//! The spawner puts a pipe on some descriptor (or a socket pair — that is
//! what Node's `stdio: "pipe"` actually makes), sets [`ENV_VAR`] to that
//! descriptor's number, and the child calls [`exit_with_parent`].
//!
//! **Which descriptor is the spawner's choice, and it matters.** Handing
//! a child the pipe on stdin is fine when the spawner knows the program —
//! the gateway's applets, the desktop shell's `datalib-http`. For a
//! program it does not know, stdin is the wrong place: a program that
//! reads stdin expecting the immediate end-of-file `/dev/null` gives
//! would block forever on a pipe nobody writes to. Such a spawner passes
//! the pipe on a spare descriptor and leaves stdin alone — the DAG runner
//! gives every step fd 3.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::os::fd::{BorrowedFd, FromRawFd};
use std::os::unix::fs::FileTypeExt;

/// Set by a spawner to the number of the descriptor holding the pipe:
/// `0` when that is stdin, `3` for a spare one.
///
/// An opt-in rather than the default, because a program run by hand has
/// no such descriptor at all, and on stdin it would have a terminal
/// (reading it would swallow the user's input) or `/dev/null` (an
/// immediate EOF would look like a dead parent).
pub const ENV_VAR: &str = "DATALIB_PARENT_PIPE";

/// [`ENV_VAR`] was set to something that is not a parent pipe.
///
/// Refusing to start is deliberate. The alternative — carrying on with no
/// watch — is a protection that is silently absent, which is the failure
/// this crate exists to prevent.
#[derive(Debug)]
pub struct NotAPipe {
    detail: String,
}

impl fmt::Display for NotAPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ENV_VAR}: {}", self.detail)
    }
}

impl std::error::Error for NotAPipe {}

/// Start watching for the parent to go away if the spawner asked for it;
/// call `on_parent_gone` from a background thread when it does.
///
/// Returns `Ok(())` without doing anything when [`ENV_VAR`] is unset.
/// The callback runs on a thread of its own, after the parent is already
/// dead, so it must not write to stderr with anything that panics on a
/// failed write — see [`report`].
pub fn exit_with_parent<F>(on_parent_gone: F) -> Result<(), NotAPipe>
where
    F: FnOnce() + Send + 'static,
{
    let Some(value) = std::env::var_os(ENV_VAR) else {
        return Ok(());
    };
    let fd = parse_fd(&value.to_string_lossy())?;

    // Safety: borrowed, not owned — a failed check must not close a
    // descriptor this process is still using. `try_clone_to_owned` dups
    // it, and the dup is what gets dropped.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let kind = borrowed
        .try_clone_to_owned()
        .map(File::from)
        .and_then(|f| f.metadata())
        .map(|m| m.file_type());
    match kind {
        Ok(t) if t.is_fifo() || t.is_socket() => {}
        Ok(t) => {
            return Err(NotAPipe {
                detail: format!(
                    "fd {fd} is {}, not a pipe: the spawner asked for a parent \
                     watch and did not provide the pipe it watches",
                    describe(t)
                ),
            })
        }
        Err(e) => {
            return Err(NotAPipe {
                detail: format!("fd {fd} is unreadable ({e})"),
            })
        }
    }

    std::thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            // Never dropped: closing the descriptor is not ours to do,
            // and the loop only ends when the process is about to go.
            let mut pipe = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
            let mut scratch = [0u8; 64];
            loop {
                match pipe.read(&mut scratch) {
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

/// The value names a descriptor. stdout and stderr are refused by number
/// rather than by inspection: both are usually pipes, so the check below
/// would pass and the watch would read the wrong end of the child's own
/// output.
fn parse_fd(value: &str) -> Result<i32, NotAPipe> {
    let fd: i32 = value.trim().parse().map_err(|_| NotAPipe {
        detail: format!("expected the number of the descriptor holding the pipe, got {value:?}"),
    })?;
    match fd {
        0 | 3.. => Ok(fd),
        1 | 2 => Err(NotAPipe {
            detail: format!(
                "fd {fd} is this process' {}, never a parent pipe",
                if fd == 1 { "stdout" } else { "stderr" }
            ),
        }),
        _ => Err(NotAPipe {
            detail: format!("fd {fd} is not a descriptor"),
        }),
    }
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
