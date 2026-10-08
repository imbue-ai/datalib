//! What a source that reaches a web service shares: every request through
//! `latchkey curl` with retries and stops, HTTP playback for tests, DAV,
//! and the "listed minus held" bookkeeping a resumable download keeps.

pub mod coverage;
pub mod dav;
pub mod download_params;
pub mod http;
pub mod interrupt;
pub mod latchkey;
pub mod owed;
pub mod retry;
pub mod synthesize;
