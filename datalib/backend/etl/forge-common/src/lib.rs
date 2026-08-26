//! `datalib-etl-forge-common` — shared machinery for code-forge
//! providers (GitHub, GitLab).
//!
//! The two forges model the same thing under different names: a
//! long-lived review item (pull request / merge request) on a
//! repository (repo / project), carrying threaded comments (issue
//! comments + review comments / notes + discussions). Their REST APIs
//! are close enough that both providers had independently grown the
//! same client: the same counters, the same JSON-or-error handling,
//! and the same `Link: rel=next` pagination walk, differing only in
//! the latchkey service name and the retry classifier.
//!
//! This crate owns that shared half. It sits alongside
//! `chat-common` (chat-shaped providers) and `contact-common`
//! (contact-shaped providers), which exist for the same reason.
//!
//! What stays in each provider:
//!
//!   * `BASE` — the API root, which differs per forge.
//!   * The retry classifier, when the forge needs a non-default one.
//!     GitHub does: its *primary* rate limit is a `403` with
//!     `x-ratelimit-remaining: 0` rather than a `429`.
//!   * Everything downstream of the client — the raw schema, the
//!     render path, and the provider's own row model.

pub mod client;

pub use client::{ForgeClient, ForgeError, LATCHKEY_TIMEOUT, PER_PAGE};
