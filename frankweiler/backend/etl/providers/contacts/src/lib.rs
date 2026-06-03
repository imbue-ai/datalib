//! CardDAV provider for [`frankweiler_etl`]: downloads address books
//! from any RFC 4791 / RFC 6352-compliant server (iCloud, Fastmail,
//! Google CardDAV, …) into a doltlite raw store of vCard payloads.
//!
//! One provider crate covers all three sources because the on-wire
//! protocol + data shape are identical — only the base URL and the
//! auth flavor differ, and both of those live in per-source config.
//!
//! ## Known limitations / wontfix-for-now
//!
//! * **Google data is incomplete.** Google's CardDAV surface omits
//!   custom fields, profile photos sourced from Gmail, and the merge
//!   layer their People API performs across signed-in surfaces. If
//!   completeness matters, write a parallel `google_people` provider
//!   talking to <https://people.googleapis.com>; the storage shape
//!   here is intentionally generic enough to dedupe against later.
//!
//! * **Read-only.** Translate produces grid rows; we do not push
//!   changes back to the server. The transport layer is one-way by
//!   construction (no `PUT` / `DELETE` plumbing in
//!   `frankweiler_etl::http`).
//!
//! * **One auth identity per host.** Latchkey keys credentials by URL
//!   host (`carddav.fastmail.com`, `contacts.icloud.com`, …). Two
//!   Fastmail accounts can't currently coexist; if you need that,
//!   we'll have to layer per-source basic-auth into the request
//!   instead of relying on latchkey's host map.
//!
//! * **No write-back of merge decisions.** Dedupe across the three
//!   sources is the UI's job; we store one row per (source × upstream
//!   contact) and let the front-end overlay user-authored linkages.

pub mod extract;
pub mod translate;
