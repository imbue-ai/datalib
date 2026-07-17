//! Local-filesystem vCard ingest.
//!
//! Walks a directory tree (or a single `.vcf` file) and writes every
//! contained vCard into the raw doltlite store using the same row
//! shape the CardDAV path produces. Render then has one input
//! shape regardless of where the vCards came from: a remote CardDAV
//! server, a Google "Export contacts" dump, or a test fixture.
//!
//! Synthetic identity:
//!   - `account_id` = `opts.account_id_override` or `"local"`.
//!     `server_url` is set to `file://<input_path>` so the row
//!     round-trips a meaningful provenance string without pretending
//!     it came over HTTP.
//!   - One `addressbooks` row per `.vcf` file. `display_name` =
//!     `addressbook_label` = file stem (the same convention the
//!     render path used before).
//!   - `href` for each contact = relative path within `input_path`,
//!     suffixed with the block index when a single file packs many
//!     vCards (Google's "Contacts.vcf" shape).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

use frankweiler_etl::control::DownloadControl;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;

use super::api::{vcard_fn, vcard_n_family_given, vcard_rev, vcard_uid};
use super::db::{addressbook_pk, db_path_for, RawDb};
use super::schema_raw::{synthesized_name_uid, ContactRow};

pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub input_path: PathBuf,
    /// Overrides the synthetic `account_id`. Defaults to `"local"`.
    pub account_id_override: Option<String>,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub addressbooks: usize,
    pub contacts_new: usize,
    pub contacts_updated: usize,
    /// `.vcf` files whose `(size, mtime)` matched the resume cursor and
    /// were skipped without re-parsing.
    pub files_skipped: usize,
    pub errors: usize,
}

/// `file_checkpoint` scope for the local-`.vcf` resume cursor. One
/// contacts DB serves one source, so a single feed name suffices; each
/// `.vcf` file is namespaced by its canonical path within the scope.
const CHECKPOINT_SCOPE: &str = "carddav/vcf";

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&opts.db_path).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
        // Drop the resume cursor too, so every `.vcf` re-ingests
        // rather than being skipped against a now-empty contacts table.
        file_checkpoint::clear_scope(db.pool(), CHECKPOINT_SCOPE).await?;
    }
    let _ = opts.control.refetch_blobs;

    let account_id = opts
        .account_id_override
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let server_url = format!("file://{}", opts.input_path.display());
    db.upsert_account(&account_id, &server_url, None, None)
        .await?;

    let files = collect_vcf_files(&opts.input_path)?;
    opts.progress.set_length(Some(files.len() as u64));

    // Resume cursor: `(size, mtime)` per already-ingested file. Same
    // mechanism mbox/email and the shared takeout providers use.
    let stamped = file_checkpoint::load(db.pool(), CHECKPOINT_SCOPE).await?;

    let mut summary = FetchSummary::default();
    for path in &files {
        // One `stat` up front. A file we can't stat is treated as
        // changed (fall through to ingest, which surfaces the real
        // read error if the path is genuinely broken).
        let fingerprint = match FileFingerprint::of(path) {
            Ok(fp) => Some(fp),
            Err(e) => {
                warn!(event = "carddav_vcf_stat_failed", path = %path.display(), error = %e);
                None
            }
        };
        if let Some(fp) = &fingerprint {
            if file_checkpoint::should_skip(&stamped, fp) {
                info!(
                    event = "carddav_vcf_skipped",
                    path = %path.display(),
                    size_bytes = fp.size_bytes,
                    "fingerprint matches checkpoint; skipping re-ingest",
                );
                summary.files_skipped += 1;
                opts.progress.inc(1);
                continue;
            }
        }

        opts.progress
            .set_message(&format!("ingesting {}", path.display()));
        match ingest_one(&db, &opts.input_path, path, &account_id, &mut summary).await {
            Ok(()) => {
                // Stamp only after a clean ingest, so a crash mid-file
                // leaves no cursor and the next run re-ingests it.
                if let Some(fp) = &fingerprint {
                    file_checkpoint::record_finished_pool(db.pool(), CHECKPOINT_SCOPE, fp).await?;
                }
            }
            Err(e) => {
                summary.errors += 1;
                warn!(
                    event = "carddav_vcf_ingest_failed",
                    path = %path.display(),
                    error = %e,
                );
            }
        }
        opts.progress.inc(1);
    }

    // Lift inline vCard photos into the per-source CAS (consistent
    // contact_photos shape, shared with the LinkedIn provider). `db_path`
    // is the per-source *directory*, so resolve it to the entity db file
    // before deriving the CAS sibling — otherwise `cas_path_for` walks up
    // to the shared `raw/` parent and the store leaks to
    // `raw/blobs.doltlite_db` (mirrors the CardDAV path in `mod.rs`).
    if let Err(e) = super::photos::lift_photos_to_cas(&db, &db_path_for(&opts.db_path)).await {
        warn!(event = "carddav_vcf_photo_lift_failed", error = %e);
    }
    Ok(summary)
}

async fn ingest_one(
    db: &RawDb,
    root: &Path,
    file: &Path,
    account_id: &str,
    summary: &mut FetchSummary,
) -> Result<()> {
    let body = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let label = addressbook_label(file);
    let book_href = relative_href(root, file);
    let book_id = addressbook_pk(account_id, &book_href);
    db.upsert_addressbook(account_id, &book_href, Some(&label), None, None)
        .await?;
    summary.addressbooks += 1;

    let existing = db.contact_uids(&book_id).await?;
    let mut rows: Vec<ContactRow> = Vec::new();
    // Synthesized name-based ids seen so far in *this* file, mapped to a
    // human label, so we can warn when two distinct cards collapse onto
    // the same id (e.g. two people named "John Smith").
    let mut synth_seen: HashMap<String, String> = HashMap::new();
    for (idx, block) in split_vcards(&body).into_iter().enumerate() {
        let href = if idx == 0 {
            book_href.clone()
        } else {
            format!("{book_href}#{idx}")
        };
        let uid = contact_uid(file, &label, idx, &block, &mut synth_seen);
        if existing.contains(&uid) {
            summary.contacts_updated += 1;
        } else {
            summary.contacts_new += 1;
        }
        rows.push(ContactRow::new(
            book_id.clone(),
            uid,
            href,
            None,
            vcard_fn(&block),
            vcard_rev(&block),
            &block,
        ));
    }
    db.upsert_contacts(&rows).await?;
    Ok(())
}

/// Stable id for one vCard, in priority order:
///
/// 1. The RFC 6350 `UID:` if the card has one (the proper ship-of-
///    Theseus identity; CardDAV servers and well-formed exports emit it).
/// 2. Otherwise, when the card has a name, a UUIDv5 synthesized from
///    first + last name ([`synthesized_name_uid`]) so the same person
///    survives re-export. Two cards that collapse onto one synthesized
///    id are flagged via `synth_seen`.
/// 3. Otherwise (no UID, no name — ~20% of Google's export), the file
///    position. Can't anchor identity to anything stable, but keeps
///    distinct nameless cards from collapsing into one row. Warned so
///    the loss of permanence is visible rather than silent.
fn contact_uid(
    file: &Path,
    label: &str,
    idx: usize,
    block: &str,
    synth_seen: &mut HashMap<String, String>,
) -> String {
    if let Some(uid) = vcard_uid(block) {
        return uid;
    }
    // No UID: try first + last name. Fall back to `FN` (whole formatted
    // name in the "given" slot) when the structured `N` line is absent.
    let (family, given) = vcard_n_family_given(block)
        .or_else(|| vcard_fn(block).map(|fnv| (String::new(), fnv)))
        .unwrap_or_default();
    if given.trim().is_empty() && family.trim().is_empty() {
        warn!(
            event = "carddav_vcf_nameless_contact",
            path = %file.display(),
            index = idx,
            "vCard has no UID and no name; keying on file position — \
             object permanence not available for this contact",
        );
        return format!("{label}:{}:{idx}", file_stem_or_anon(file));
    }
    let uid = synthesized_name_uid(&given, &family);
    let display_name =
        vcard_fn(block).unwrap_or_else(|| format!("{given} {family}").trim().to_string());
    if let Some(prev) = synth_seen.insert(uid.clone(), display_name.clone()) {
        warn!(
            event = "carddav_vcf_synth_uid_collision",
            path = %file.display(),
            uid = %uid,
            name = %display_name,
            collides_with = %prev,
            "two vCards share a first+last name and collapse onto one \
             synthesized id; one will overwrite the other",
        );
    }
    uid
}

fn collect_vcf_files(input: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if input.is_file() {
        if input.extension().and_then(|s| s.to_str()) == Some("vcf") {
            out.push(input.to_path_buf());
        }
    } else if input.is_dir() {
        walk(input, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk(&p, out)?;
        } else if p.extension().and_then(|s| s.to_str()) == Some("vcf") {
            out.push(p);
        }
    }
    Ok(())
}

fn addressbook_label(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "default".to_string())
}

fn file_stem_or_anon(path: &Path) -> &str {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("anon")
}

fn relative_href(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .and_then(|p| p.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            file.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("contacts.vcf")
                .to_string()
        })
}

/// Split a `.vcf` body into individual `BEGIN:VCARD…END:VCARD`
/// blocks. Tolerates CRLF / LF / mixed line endings and case-
/// insensitive markers. Text outside a block is dropped.
fn split_vcards(body: &str) -> Vec<String> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(String::new());
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
        if trimmed.eq_ignore_ascii_case("END:VCARD") {
            if let Some(buf) = current.take() {
                out.push(buf);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Production shape: the processor passes the per-source *directory* as
    // `db_path` (e.g. `raw/fastmail_contacts`) and a `db` already opened at
    // its `entities.doltlite_db`. The inline-photo CAS must land beside that
    // entity db — `raw/fastmail_contacts/blobs.doltlite_db` — not one level
    // up in the shared `raw/` root. Passing the bare dir to `cas_path_for`
    // (which derives the sibling via `.parent()`) leaks the store to
    // `raw/blobs.doltlite_db`; this pins it to the per-source dir.
    #[tokio::test]
    async fn inline_photo_cas_lands_in_per_source_dir_not_parent() {
        // The shared raw root and the per-source dir within it.
        let raw_root = tempfile::tempdir().unwrap();
        let source_dir = raw_root.path().join("fastmail_contacts");
        std::fs::create_dir_all(&source_dir).unwrap();

        // A Google/Fastmail-style export dir with one inline-photo vCard
        // (Picard's comm-badge mugshot, the PNG from the photo-decode test).
        let export = tempfile::tempdir().unwrap();
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        std::fs::write(
            export.path().join("Bridge.vcf"),
            format!(
                "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\n\
                 PHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
            ),
        )
        .unwrap();

        // Open the db where the processor would: the entity db inside the dir.
        let entity_db = db_path_for(&source_dir);
        let db = RawDb::open(&entity_db).await.unwrap();
        let summary = fetch(FetchOptions {
            db_path: source_dir.clone(),
            db: Some(db),
            input_path: export.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        })
        .await
        .unwrap();
        assert_eq!(summary.contacts_new, 1);

        assert!(
            source_dir.join("blobs.doltlite_db").exists(),
            "photo CAS must sit beside entities.doltlite_db in the source dir",
        );
        assert!(
            !raw_root.path().join("blobs.doltlite_db").exists(),
            "photo CAS must not leak into the shared raw/ parent",
        );
    }

    #[tokio::test]
    async fn fetch_walks_directory_and_writes_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Bridge.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let opts = || FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        };
        let summary = fetch(opts()).await.unwrap();
        assert_eq!(summary.contacts_new, 2);
        assert_eq!(summary.addressbooks, 1);
        assert_eq!(summary.files_skipped, 0);

        let db = RawDb::open(&db_path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 2);

        // Second run over the unchanged file skips it via the resume
        // cursor — no contacts re-classified as new or updated.
        let again = fetch(opts()).await.unwrap();
        assert_eq!(again.files_skipped, 1);
        assert_eq!(again.contacts_new, 0);
        assert_eq!(again.contacts_updated, 0);
    }

    #[tokio::test]
    async fn fetch_reingests_when_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let vcf = dir.path().join("Bridge.vcf");
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let opts = || FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        };
        let first = fetch(opts()).await.unwrap();
        assert_eq!(first.contacts_new, 1);

        // Rewrite with an extra contact: size changes, so the cursor
        // misses and the whole file re-ingests. The pre-existing contact
        // reports as an update, the added one as new.
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.files_skipped, 0);
        assert_eq!(second.contacts_new, 1);
        assert_eq!(second.contacts_updated, 1);
    }

    // Google's vCard export carries no `UID:` — identity rides the
    // first+last name instead. An edit to a *non-name* field must keep
    // the same row (the ship-of-Theseus property the synthesized id
    // buys us), not fork into a new contact.
    #[tokio::test]
    async fn synthesized_name_id_survives_field_edit_for_uidless_cards() {
        let dir = tempfile::tempdir().unwrap();
        let vcf = dir.path().join("Google.vcf");
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada Lovelace\nN:Lovelace;Ada;;;\nEMAIL:ada@x.org\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let opts = || FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        };
        let first = fetch(opts()).await.unwrap();
        assert_eq!(first.contacts_new, 1);

        // Edit the email (and grow the file so the resume cursor misses
        // and re-ingests). Name is unchanged → same synthesized id →
        // update, not a second row.
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada Lovelace\nN:Lovelace;Ada;;;\nEMAIL:ada.lovelace@analytical.org\nEND:VCARD\n",
        )
        .unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.files_skipped, 0);
        assert_eq!(second.contacts_new, 0);
        assert_eq!(second.contacts_updated, 1);

        let db = RawDb::open(&db_path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "edited contact stayed one row, not two");
    }

    // Two UID-less cards sharing a first+last name collapse onto one
    // synthesized id (the documented collision). We don't lose the file,
    // but the rows merge — assert the collapse so the behavior is pinned.
    #[tokio::test]
    async fn same_name_uidless_cards_collapse_to_one_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Google.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nFN:John Smith\nN:Smith;John;;;\nEMAIL:john1@x.org\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nFN:John Smith\nN:Smith;John;;;\nEMAIL:john2@x.org\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let summary = fetch(FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        })
        .await
        .unwrap();
        assert_eq!(summary.addressbooks, 1);

        let db = RawDb::open(&db_path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "same-name cards share a synthesized id");
    }

    // A card with neither UID nor name keeps file-position identity so
    // distinct nameless cards don't collapse into a single row.
    #[tokio::test]
    async fn nameless_uidless_cards_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Google.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nEMAIL:a@x.org\nCATEGORIES:myContacts\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nEMAIL:b@x.org\nCATEGORIES:myContacts\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let summary = fetch(FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        })
        .await
        .unwrap();
        assert_eq!(summary.contacts_new, 2);

        let db = RawDb::open(&db_path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 2, "two nameless cards stayed distinct rows");
    }
}
