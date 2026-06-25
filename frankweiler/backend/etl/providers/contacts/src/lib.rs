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
//! * **Google goes through a different surface.** Latchkey ships a
//!   built-in `google-people` service that handles Google's OAuth2
//!   dance for the People API, but it has no `google-carddav`
//!   entry. Google's CardDAV endpoint also returns strictly less
//!   data than People (no custom fields, no Gmail-merged photos),
//!   so the right move when we want Google is a separate
//!   `frankweiler_etl_google_people` crate against
//!   <https://people.googleapis.com>, not extending this one. The
//!   storage shape here is intentionally generic enough to dedupe
//!   against that later.
//!
//! * **Apple + Fastmail need a one-time latchkey registration.**
//!   Neither host is a latchkey built-in. Register once with:
//!
//!   ```sh
//!   latchkey services register apple-contacts \
//!       --base-api-url https://contacts.icloud.com/
//!   latchkey auth set apple-contacts \
//!       -H "Authorization: Basic $(echo -n appleid:app-password | base64)"
//!   ```
//!
//!   The same shape works for `fastmail-contacts` against
//!   `https://carddav.fastmail.com/` with an app password from
//!   Fastmail's settings → "App passwords" (Contacts read-only).
//!
//! * **Translate-only mode (no live CardDAV).** Drop a `.vcf`
//!   export (Google "Export contacts", Fastmail bulk export, etc.)
//!   on disk and point a translate-only source at it:
//!
//!   ```yaml
//!   - name: contacts
//!     source:
//!       type: carddav
//!       common:
//!         input_path: ~/Downloads/contacts.vcf
//!   ```
//!
//!   No `sync:` block ⇒ extract is skipped; the translate path
//!   reads the file directly. See
//!   [`render_and_index_md::parse`] for the directory-vs-file + multi-block
//!   semantics.
//!
//! * **Read-only.** Translate produces grid rows; we do not push
//!   changes back to the server. The transport layer is one-way by
//!   construction (no `PUT` / `DELETE` plumbing in
//!   `frankweiler_etl::http`).
//!
//! * **One auth identity per host.** Latchkey keys credentials by
//!   service-name, which maps 1:1 to a URL pattern. Two Fastmail
//!   accounts on the same host can't currently coexist; if you
//!   need that, register two services with different names and
//!   matching base URLs.
//!
//! * **No write-back of merge decisions.** Dedupe across sources
//!   is the UI's job; we store one row per (source × upstream
//!   contact) and let the front-end overlay user-authored
//!   linkages.

pub mod extract;
pub mod processor;
pub mod render_and_index_md;
