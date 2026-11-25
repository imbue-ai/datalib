//! datalib's downloaders against the fake Slack, Linear and Google of
//! github.com/imbue-ai/fake-services, reached through the real latchkey at
//! the real URLs. One module per downloader.
//!
//! The seeds are small Starfleet workspaces of the tests' own. A Slack
//! seed is served shifted to the run's own day (`to_trial_start`), since
//! the Slack download measures its refresh window and its reply search
//! from the wall clock; the Linear and Google seeds are served as written.

mod calendar;
mod gmail;
mod linear;
mod slack;
