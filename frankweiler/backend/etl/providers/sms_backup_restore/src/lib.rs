//! "SMS Backup & Restore" (Android) data-export provider.
//!
//! The popular [SMS Backup & Restore](https://synctech.com.au/) app
//! writes two XML files per backup: `sms-<ts>.xml` (a `<smses>` tree of
//! `<sms>` + `<mms>` records) and `calls-<ts>.xml` (a `<calls>` tree of
//! `<call>` records). [`download`] walks an export directory, ingests
//! every `<sms>` / `<mms>` / `<call>` into its own `(id, payload)` raw
//! row, and stores MMS attachment bytes (images, audio recordings, …)
//! as content-addressed blobs in the CAS with a per-message edge.
//! [`render`] renders the texts + calls as one chat per phone number
//! via the shared chat renderer (calls fold in as inline system notes,
//! mirroring Google Voice).
//!
//! Every row's PK is a uuidv5 over the most stable parsed fields, so
//! re-exporting and re-ingesting a fresh backup upserts in place rather
//! than duplicating — see [`download::schema_raw`].
//!
//! Wired into the config-driven `sync` orchestrator as the
//! `sms_backup_restore` source type:
//!
//! ```yaml
//! - name: sms_backup_restore
//!   source:
//!     type: sms_backup_restore
//!     common:
//!       input_path: ~/backups/SMSBackupRestore
//! ```

pub mod download;
pub mod processor;
pub mod render;
