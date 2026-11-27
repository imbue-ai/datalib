# `datalib_step_protocol`: the step protocol, for a step in Rust

Datalib's runner (`datalib-pipeline`) runs each step as a program of its
own and talks to it through the step protocol: environment variables, a
verb appended to the command line, and JSON lines on stdout ending in an
`outcome` line. The protocol is
[`docs/dev/step_protocol.md`](../../../docs/dev/step_protocol.md), and a
program in any language can speak it. This crate is that protocol for a
step written in Rust: the types of every line a step may print, the
reading of what the runner hands it, and nothing about Datalib's own
stores or its own step program.

It speaks protocol version 1 (`PROTOCOL_VERSION`), the `step_protocol`
a package's manifest declares. Its dependencies are `serde`,
`serde_json` and `strum`, the last only for the `strum::VariantArray`
trait, with no derive macros; it has no Datalib dependency. Datalib's
runner reads a step's stdout with it, and `datalib-step`, the step
program Datalib ships, writes its own with it.

## A whole step

The step below is built as `:minimal_step` and run under the real
runner by the runner's own tests (`rust_example_step.rs` in `dag_tests`),
and a test checks that this README holds it as written.

```rust
//! A step that notes which of its inputs moved and reports a content
//! version: the Rust counterpart of the Python step in
//! `docs/dev/step_protocol.md` §"Minimal examples".

use std::io::Write;

use datalib_step_protocol::{tree_version, Emitter, Invocation, LogLevel, Outcome, Output, Verb};

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Params {
    greeting: String,
}

fn main() {
    let invocation = match Invocation::from_env() {
        Ok(invocation) => invocation,
        Err(e) => fail(2, &e),
    };
    let out = Emitter::stdout(&invocation.step);
    if let Err(e) = run(&invocation, &out) {
        fail(1, &*e);
    }
}

/// A failing step's last lines on stderr become its error message.
fn fail(code: i32, e: &dyn std::error::Error) -> ! {
    let _ = writeln!(std::io::stderr(), "error: {e}");
    std::process::exit(code);
}

fn run(step: &Invocation, out: &Emitter) -> Result<(), Box<dyn std::error::Error>> {
    // Read on every invocation, so `--check` refuses what a run would.
    let params: Params = step.params()?;
    let dir = step.output_dir();
    match step.verb {
        Some(Verb::Reset) => {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            return Ok(());
        }
        Some(_) => return Ok(()),
        None => {}
    }

    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("greeting"), &params.greeting)?;
    out.progress_length(Some(step.inputs.len() as u64))?;
    for input in &step.inputs {
        if step.is_failed(input) {
            out.log(
                LogLevel::Warn,
                format!("{input} did not finish its last run; reading what it committed"),
            )?;
        }
        // Every input is changed on a first run; one with no version yet
        // has no entry in `DATALIB_READS`.
        if let (true, Some(read)) = (step.is_changed(input), step.read(input)) {
            out.log(LogLevel::Info, format!("{input} moved: {}", read.version))?;
            std::fs::write(dir.join(input.replace('/', "_")), read.version)?;
        }
        out.progress_inc(1)?;
    }

    // The same files with the same bytes give the same version, so what
    // reads this step runs again only when its output changed.
    let version = tree_version(&dir)?;
    out.outcome(&Outcome::with_outputs(vec![Output::versioned(
        &step.step, version,
    )]))?;
    Ok(())
}
```

Its config entry, with a producer upstream of it:

```toml
[[steps]]
id = "report/build"
command = "/path/to/minimal_step"
inputs = ["a/make"]
reads = "pinned"
params = { greeting = "hello" }

[[programs]]
command = "/path/to/minimal_step"
verbs = ["reset"]
```

## What is in it

- **`Invocation::from_env()`**: what the runner hands one invocation.
  The step id and the data root (both required), the run id, the
  attempt and the run's `now` (all `None` when the step is run by hand),
  the inputs, the changed and the failed inputs, `DATALIB_READS` as a
  map (`read(input)` splits an entry into its fingerprint and version),
  and the verb the runner appended, if any (`Verb`), with the rest of
  the command line in `args`. `params::<T>()` reads `DATALIB_DAG_PARAMS`
  into a type of yours; a step with no params reads `{}`.
  `Invocation::from_parts` takes the variables and arguments explicitly,
  for a test. `env` names every variable.
- **`Emitter`**: prints every line a step may print, one JSON object per
  line, flushed as it is written. Clones share one writer and a line is
  written whole under a lock, so threads may print at once. `event`
  takes any `Event` (`metric`, `progress_length`, `progress_inc`,
  `progress_message`, `log`, `hint`, `checkpoint`, `capabilities`), and
  each has a method of its own; `problem` and `covered` print the
  problem lines (`problems`); `outcome` prints the last line.
- **`Outcome`**: the outputs with their versions and rows, the failure
  kind (`FailureKind`) beside a non-zero exit, and the verbs' answers:
  `needs_rerun` for `--prepare`, `store_bytes` for `--collect` and
  `--compact`, `warnings` for `--check`. `CANCELLED_EXIT_CODE` is the
  exit code of a step that stopped because it was asked to.
- **`problems`**: the `problem` line (`Problem`), the `covered` line
  (`Covered`), and the closed sets a problem is described in (`Stage`,
  `ScopeKind`, `Outcome`, `Reason`, `Severity`).
- **`tree_version(dir)`**: a content version for an output that is a
  plain tree of files, for a step with no better one. It reads every
  file, so a store's own commit hash is the better version where there
  is one.
- **`read_line`**: one stdout line read as the runner reads it, for a
  test of a step's output, or a program that runs steps.

Every closed set of words is an enum with `as_str`, `parse` (`None` for
a word this version does not know), `ALL`, and serde in the same
spelling.

## What is not in it

- **Watching fd 3 for the runner going away.** The runner hands every
  step a pipe on the descriptor `DATALIB_PARENT_PIPE` names, and
  end-of-file on it means the runner is gone. The descriptor is only
  read; a step's lines go on stdout. `datalib_parent_watch`
  (`exit_with_parent`) watches it, with no dependencies.
- **Signals.** On a stop the runner sends SIGINT to the step's process
  group and SIGKILL fifteen seconds later (`step_protocol.md`
  §"Signals"). Rust's standard library cannot install a signal handler,
  so this crate does not: catch SIGINT with `signal-hook` or
  `tokio::signal`, stop at the next consistent point, commit, print
  `Outcome::failed(FailureKind::Cancelled, outputs)` and exit with
  `CANCELLED_EXIT_CODE`.
- **Logging on stderr.** Every stderr line is kept as a log line; a
  `tracing-subscriber` JSON line is taken apart into its message,
  level, target and fields. Use `tracing-subscriber`'s JSON format for
  that; nothing here is needed.
- **Probes.** A probe answers the app's "Check connection" and is not a
  step; its report is `ProbeReport` in `datalib_probe`.
