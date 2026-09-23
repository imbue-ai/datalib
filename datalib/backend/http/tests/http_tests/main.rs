//! The endpoint tests of `datalib-http`, one binary: each module is one
//! endpoint or one contract, and they share a link because linking the
//! server stack is most of what a test here costs. A module that has to
//! run in a process of its own — it installs the process's only tracing
//! subscriber, or it cannot be sandboxed — is a test slice in
//! BUILD.bazel: the package's test skips it, and its own target runs it
//! alone from this same binary.

mod applet;
mod auth_endpoint;
mod config_init;
mod dactal_csp;
mod dag_run_state;
mod feedback_endpoint;
mod feedback_loop;
mod lib_endpoint;
mod manage_rows;
mod pipeline_history;
mod pipeline_storage;
mod remote_media;
mod request_log;
mod runs_endpoints;
mod server_log;
mod sync_loop;
mod ui_events;
