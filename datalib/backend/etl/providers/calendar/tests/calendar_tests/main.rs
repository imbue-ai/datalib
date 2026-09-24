//! The calendar downloads driven end to end through HTTP playback, on
//! fake TNG data shaped like what the real services return.
//!
//! `RUST_TEST_THREADS=1`: the playback root is a process-global
//! environment variable each test points at its own tree.

mod caldav_playback;
mod google_playback;
