//! What the runner hands a step when it starts it: the environment and the
//! verb appended to the command line (`step_protocol.md` §"What your
//! command receives" and §"Verbs").

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::env;

crate::closed_set! {
    /// A verb the runner appends to a step's command line as `--<verb>`,
    /// only to a program whose `[[programs]]` entry declares it. An
    /// invocation with a verb does that verb's work and nothing else.
    #[derive(PartialOrd, Ord)]
    pub enum Verb {
        /// Empty what the step wrote, so its next run starts over.
        Reset = "reset",
        /// Sent each time the runner starts: bring what the step wrote to
        /// this program's shape, or say it cannot.
        Prepare = "prepare",
        /// Give back disk, dropping history nothing reads.
        Compact = "compact",
        /// Give back disk, keeping every version.
        Collect = "collect",
        /// Say whether the step would run its params as written; open and
        /// write nothing.
        Check = "check",
    }
}

impl Verb {
    /// The argument the runner appends: `--reset` for [`Verb::Reset`].
    pub fn flag(self) -> String {
        format!("--{}", self.as_str())
    }

    /// The verb a command line ends with, and the arguments before it.
    /// The runner appends at most one verb, always last, so an argument
    /// anywhere else is the program's own.
    pub fn split_args(mut args: Vec<String>) -> (Vec<String>, Option<Verb>) {
        let verb = args
            .last()
            .and_then(|last| last.strip_prefix("--"))
            .and_then(Verb::parse);
        if verb.is_some() {
            args.pop();
        }
        (args, verb)
    }
}

/// One input's entry in `DATALIB_READS`: `<fingerprint>:<version>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Read<'a> {
    /// A hash of the producing step's definition.
    pub fingerprint: &'a str,
    /// The version that step reported, such as a commit of its store.
    pub version: &'a str,
}

/// Everything the runner hands one invocation of a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `DATALIB_DAG_STEP`: the step's id, and the one tree it writes.
    pub step: String,
    /// `DATALIB_DAG_DATA_ROOT`: absolute, and the working directory.
    pub data_root: PathBuf,
    /// `DATALIB_DAG_RUN_ID`; `None` when the step is run by hand.
    pub run_id: Option<String>,
    /// `DATALIB_DAG_ATTEMPT`, from 1; `None` when the step is run by hand.
    pub attempt: Option<u32>,
    /// `DATALIB_DAG_NOW`, RFC 3339; `None` when the step is run by hand.
    pub now: Option<String>,
    /// `DATALIB_DAG_PARAMS` as written; `None` when the step has none.
    /// [`Invocation::params`] reads it into a type of yours.
    pub params_json: Option<String>,
    /// `DATALIB_DAG_INPUTS`, relative to the data root.
    pub inputs: Vec<String>,
    /// `DATALIB_DAG_CHANGED_INPUTS`: every input on a first run.
    pub changed_inputs: Vec<String>,
    /// `DATALIB_DAG_FAILED_INPUTS`: inputs to read as partial.
    pub failed_inputs: Vec<String>,
    /// `DATALIB_READS`: input → `<fingerprint>:<version>`. An input with
    /// no version yet has no entry.
    pub reads: BTreeMap<String, String>,
    /// The command line after the program's name, without the verb.
    pub args: Vec<String>,
    /// The verb the runner appended, if this invocation is one.
    pub verb: Option<Verb>,
}

/// A variable the runner sets that is missing or does not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationError {
    pub variable: &'static str,
    pub problem: String,
}

impl std::fmt::Display for InvocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.variable, self.problem)
    }
}

impl std::error::Error for InvocationError {}

/// `DATALIB_DAG_PARAMS` does not read as the type asked for.
#[derive(Debug)]
pub struct ParamsError(pub serde_json::Error);

impl std::fmt::Display for ParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", env::DATALIB_DAG_PARAMS, self.0)
    }
}

impl std::error::Error for ParamsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl Invocation {
    /// This process's invocation: its environment and its command line.
    pub fn from_env() -> Result<Invocation, InvocationError> {
        let args = std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        Invocation::from_parts(|name| std::env::var_os(name), args)
    }

    /// An invocation from a variable lookup and the arguments after the
    /// program's name: what [`Invocation::from_env`] reads, given
    /// explicitly, as a test would.
    pub fn from_parts(
        var: impl Fn(&str) -> Option<OsString>,
        args: Vec<String>,
    ) -> Result<Invocation, InvocationError> {
        let text = |name: &'static str| -> Result<Option<String>, InvocationError> {
            match var(name) {
                None => Ok(None),
                Some(v) => v.into_string().map(Some).map_err(|_| InvocationError {
                    variable: name,
                    problem: "is not UTF-8".into(),
                }),
            }
        };
        let lines = |name: &'static str| -> Result<Vec<String>, InvocationError> {
            Ok(text(name)?
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect())
        };
        let required = |name: &'static str| InvocationError {
            variable: name,
            problem: "is not set; the runner sets it for every step, and a step run by \
                      hand needs it set"
                .into(),
        };

        let step = text(env::DATALIB_DAG_STEP)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| required(env::DATALIB_DAG_STEP))?;
        let data_root = var(env::DATALIB_DAG_DATA_ROOT)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| required(env::DATALIB_DAG_DATA_ROOT))?;
        let attempt = match text(env::DATALIB_DAG_ATTEMPT)? {
            None => None,
            Some(a) => Some(a.parse().map_err(|_| InvocationError {
                variable: env::DATALIB_DAG_ATTEMPT,
                problem: format!("{a:?} is not a whole number"),
            })?),
        };
        let reads = match text(env::DATALIB_READS)? {
            None => BTreeMap::new(),
            Some(r) if r.trim().is_empty() => BTreeMap::new(),
            Some(r) => serde_json::from_str(&r).map_err(|e| InvocationError {
                variable: env::DATALIB_READS,
                problem: format!("is not a JSON object of strings: {e}"),
            })?,
        };
        let (args, verb) = Verb::split_args(args);
        Ok(Invocation {
            step,
            data_root,
            run_id: text(env::DATALIB_DAG_RUN_ID)?,
            attempt,
            now: text(env::DATALIB_DAG_NOW)?,
            params_json: text(env::DATALIB_DAG_PARAMS)?,
            inputs: lines(env::DATALIB_DAG_INPUTS)?,
            changed_inputs: lines(env::DATALIB_DAG_CHANGED_INPUTS)?,
            failed_inputs: lines(env::DATALIB_DAG_FAILED_INPUTS)?,
            reads,
            args,
            verb,
        })
    }

    /// The step's params read into `T`. A step with no params reads `{}`,
    /// so a type whose fields all have defaults needs none set.
    pub fn params<T: serde::de::DeserializeOwned>(&self) -> Result<T, ParamsError> {
        serde_json::from_str(self.params_json.as_deref().unwrap_or("{}")).map_err(ParamsError)
    }

    /// The directory this step writes: `<data_root>/<step>`.
    pub fn output_dir(&self) -> PathBuf {
        self.data_root.join(&self.step)
    }

    /// The version `input` was started against, split at its first `:`.
    /// `None` for an input with no version yet, or an entry with no `:`.
    pub fn read(&self, input: &str) -> Option<Read<'_>> {
        let (fingerprint, version) = self.reads.get(input)?.split_once(':')?;
        Some(Read {
            fingerprint,
            version,
        })
    }

    pub fn is_changed(&self, input: &str) -> bool {
        self.changed_inputs.iter().any(|i| i == input)
    }

    pub fn is_failed(&self, input: &str) -> bool {
        self.failed_inputs.iter().any(|i| i == input)
    }
}
