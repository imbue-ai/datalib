//! The names of the environment variables the runner sets for a step
//! (`step_protocol.md` §"What your command receives"). [`Invocation`]
//! reads them; the runner sets them by these names.
//!
//! [`Invocation`]: crate::Invocation

macro_rules! names {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $($(#[$meta])* pub const $name: &str = stringify!($name);)*
        /// Every variable the runner sets for a step.
        pub const ALL: &[&str] = &[$($name),*];
    };
}

names! {
    /// The step's id, and the one tree under the data root it writes.
    DATALIB_DAG_STEP,
    /// The run this invocation belongs to.
    DATALIB_DAG_RUN_ID,
    /// Which attempt this is, from `1`.
    DATALIB_DAG_ATTEMPT,
    /// The data root, absolute; also the step's working directory.
    DATALIB_DAG_DATA_ROOT,
    /// The step's `params`, as JSON; unset when it has none.
    DATALIB_DAG_PARAMS,
    /// The step's inputs, one per line, relative to the data root.
    DATALIB_DAG_INPUTS,
    /// The inputs whose version moved since the step last succeeded.
    DATALIB_DAG_CHANGED_INPUTS,
    /// The inputs whose producer's last run did not succeed.
    DATALIB_DAG_FAILED_INPUTS,
    /// A JSON object: input → the version the step was started against.
    DATALIB_READS,
    /// The run's timestamp (RFC 3339), for every time the step records.
    DATALIB_DAG_NOW,
}
