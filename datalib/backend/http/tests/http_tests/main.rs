//! The hermetic endpoint tests of `datalib-http`, one binary: each
//! module is one endpoint or one contract, and they share a link
//! because linking the server stack is most of what a test here costs.
//! A test that installs the process's tracing subscriber cannot live
//! here (`server_log.rs`, `request_log.rs`, `ui_events.rs`), nor one
//! that cannot be sandboxed (`applet_tests/`).

mod auth_endpoint;
mod config_init;
mod dactal_csp;
mod dag_run_state;
mod feedback_endpoint;
mod lib_endpoint;
mod manage_rows;
mod pipeline_history;
mod pipeline_storage;
mod remote_media;
mod runs_endpoints;
mod worker_cancel;
mod worker_failure_tail;
