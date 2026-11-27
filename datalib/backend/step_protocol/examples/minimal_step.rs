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
