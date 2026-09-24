//! The CardDAV download driven end to end through HTTP playback, on the
//! TNG address books served the way Fastmail serves them.
//!
//! `RUST_TEST_THREADS=1`: the playback root is a process-global
//! environment variable each test points at its own tree.

mod carddav_playback;
