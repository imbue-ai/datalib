# Doltlite fork() bug — minimal reproducer

doltlite's chunk-store coordination uses **BSD `flock(2)`** on a
separately-`open()`ed file descriptor. BSD flock state lives on the
underlying *open file description* (OFD), not the fd number. When a
process holding such an fd calls `fork()` (or `posix_spawn()`), the
child inherits a duplicate fd pointing at the same OFD — and the
flock stays held until *all* fds referring to it are closed, **across
both parent and child**.

This means: if any thread in a process forks while another thread is
mid-`INSERT` on a doltlite database (which transiently holds the
chunk-store flock), the parent's subsequent `close()` of its own fd
does not release the lock — the child still holds an inherited copy.
The parent's next `INSERT` or `dolt_commit` fails with `SQLITE_BUSY`
until the child exits.

## What this reproducer does

`fork_vs_db.c` is a 100-line C program that runs two pthreads:

- **Writer thread**: in a tight loop, `INSERT`s a row through a
  persistent prepared statement, counting `SQLITE_BUSY` returns.
- **Forker thread**: in a tight loop, `posix_spawn`s `/bin/sleep 0.05`.
  This is exactly the syscall shape Rust's `std::process::Command::spawn()`
  uses on macOS — and what frankweiler-sync's `latchkey_curl` HTTP
  transport does on every HTTPS call.

`run.sh` downloads both amalgamations fresh from their canonical
upstream URLs (verifying sha256), then compiles this same source file
twice — once against stock SQLite 3.51.0, once against doltlite
v0.11.5 — and runs both with identical wall-clock parameters.

Downloading from upstream (rather than bundling) keeps provenance
self-evident: the amalgamations are byte-for-byte what dolthub and
sqlite.org publish.

## To run

```sh
./run.sh                  # 3-second runs each (default)
SECONDS_TO_RUN=5 ./run.sh  # longer if signal is faint
```

Requires network access on first run to fetch the amalgamation zips
into `./_work/` (cached for subsequent runs).

## Expected result

Same C program, same workload, same fork pattern:

| build | inserts_ok | **inserts_BUSY** |
|---|---|---|
| stock SQLite 3.51.0 | thousands | **0** |
| doltlite v0.11.5 | thousands | **hundreds-to-hundreds-of-thousands** |

Numbers vary per run and per system but the qualitative gap is
reliable. Stock SQLite is unaffected; doltlite reliably fails.

## Why SQLite is immune but doltlite is not

Stock SQLite's default Unix VFS uses **POSIX `fcntl(F_SETLK)`**
byte-range locks. Per POSIX, those locks are *process-owned*; the
child does **not** inherit the parent's locks via `fork()`, and the
parent's `close()` releases its own locks immediately. Doltlite layers
its own chunk-store coordination on top using **BSD `flock(LOCK_EX|LOCK_NB)`**,
which has OFD ownership semantics and *is* inheritable by fork.

| primitive | held by | survives `fork()`? | released by parent's `close()`? |
|---|---|---|---|
| **POSIX `fcntl(F_SETLK)`** (SQLite uses this) | the process | no | yes (process-local) |
| **BSD `flock`** (doltlite uses this) | the OFD | yes — child inherits | only when *all* fds across *all* processes close |

## Where the choice is made in doltlite

`csFileLock` and `csFileLockNB` in `chunk_store.c` (in the doltlite
amalgamation; search for the function names). Both call
`open(path, O_RDWR | O_CREAT, 0644)` followed by `flock(fd, LOCK_EX | …)`.

## Why this is hard to debug downstream

The OFD reference held in the forked child is invisible to `lsof` on
the parent's pid (you only see the child if you also pass `-p`
with the child pid, which is often gone by the time you check). It
also doesn't show up in any SQLite-level diagnostic — by the time
doltlite's `chunkStoreLockAndRefresh` returns `SQLITE_BUSY` from
inside `dolt_commit` execution, the SQLite engine reports it as
"database is locked" with no further context.

## Suggested fixes

In order of cost vs. completeness:

1. **`O_CLOEXEC` on the lock fd.** Mitigates the bug under
   `fork()+exec()` patterns where the child execs quickly — but does
   *not* close the inheritance window between `fork()` and `execve()`,
   so concurrent workloads can still hit it. (We observed ~99.9%
   reduction in BUSY rate but not zero.)
2. **Switch the chunk-store lock to `fcntl(F_SETLK)` byte-range locks**
   so it inherits stock SQLite's fork-safety. This matches SQLite's
   own choice and eliminates the bug class entirely. The chunk-store
   would have to pick byte offsets that don't conflict with SQLite's
   own use (`PENDING_BYTE` / `RESERVED_BYTE` / `SHARED_SIZE`) but
   that's a fixed assignment.
3. **Document the constraint loudly**: "do not fork while any thread
   in your process holds a doltlite connection." This is consistent
   with SQLite's existing
   [How to Corrupt Your Database](https://www.sqlite.org/howtocorrupt.html)
   section 2.5 warning, but doltlite's flock makes the failure
   *visible* (loud `SQLITE_BUSY`) rather than silent (database
   corruption), which is arguably better — but it still surprises
   any user with a tokio runtime that might spawn children for
   unrelated reasons.

## Environment

- macOS (builds with any system clang on any recent macOS or Linux
  with pthread).
- doltlite v0.11.5 amalgamation, fetched at runtime from
  `https://github.com/dolthub/doltlite/releases/download/v0.11.5/doltlite-amalgamation-0.11.5.zip`
  (sha256 `c9b6f4dbf46b5fa6c2a8a889ed862997f3b48b03ee745051bb6e9f4008ba66b0`).
- SQLite 3.51.0 amalgamation, fetched at runtime from
  `https://www.sqlite.org/2025/sqlite-amalgamation-3510000.zip`
  (sha256 `1caf7116f2910600d04473ad69d37ec538fa62fa36adccd37b5e0e43647c98be`).

`run.sh` verifies both checksums before building.
