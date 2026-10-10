//! Every GitLab test, one binary: each module is one behaviour.

mod interrupt;
mod live;
mod playback_roundtrip;
mod support;
mod upgrade;

/// The fixture's pinned clock, as the pipeline's `--now` sets it.
pub fn tng_now() -> datalib_time::IsoOffsetTimestamp {
    datalib_time::parse_strict("2369-04-15T00:00:00+00:00").unwrap()
}
