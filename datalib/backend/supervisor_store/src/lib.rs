//! What the server and the runner's loop say to each other, and nothing of
//! the loop itself: the store in `system/supervisor.sqlite` (requests,
//! jobs, turn-offs, and what the loop records of each step), the
//! announcements in `system/supervisor-listeners/`, the lines
//! `datalib-pipeline serve` prints on its stderr, and `runner-lock`. The
//! runner (`datalib_dag`) and the server (`datalib_http`) both link it.
//! The rules are `dag/README.md` §"The record", §"What wakes the loop" and
//! §"Two locks, two files".

pub mod announce;
pub mod lock;
pub mod pass;
pub mod record;
pub mod run_state;
pub mod serve_event;
pub mod state;
pub mod store;
