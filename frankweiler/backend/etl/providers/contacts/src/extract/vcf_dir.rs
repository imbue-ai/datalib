//! Local-filesystem vCard ingest.
//!
//! Walks a directory tree (or a single `.vcf` file) and writes every
//! contained vCard into the raw doltlite store using the same row
//! shape the CardDAV path produces. Translate then has one input
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
//!     translate path used before).
//!   - `href` for each contact = relative path within `input_path`,
//!     suffixed with the block index when a single file packs many
//!     vCards (Google's "Contacts.vcf" shape).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;

use super::api::{vcard_fn, vcard_rev, vcard_uid};
use super::db::{addressbook_pk, RawDb};
use super::schema_raw::ContactRow;

pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub input_path: PathBuf,
    /// Overrides the synthetic `account_id`. Defaults to `"local"`.
    pub account_id_override: Option<String>,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub addressbooks: usize,
    pub contacts_new: usize,
    pub contacts_updated: usize,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&opts.db_path).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
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

    let mut summary = FetchSummary::default();
    for path in &files {
        opts.progress
            .set_message(&format!("ingesting {}", path.display()));
        if let Err(e) = ingest_one(&db, &opts.input_path, path, &account_id, &mut summary).await {
            summary.errors += 1;
            warn!(
                event = "carddav_vcf_ingest_failed",
                path = %path.display(),
                error = %e,
            );
        }
        opts.progress.inc(1);
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

    let existing = db.contact_etags_by_href(&book_id).await?;
    let mut rows: Vec<ContactRow> = Vec::new();
    for (idx, block) in split_vcards(&body).into_iter().enumerate() {
        let uid = vcard_uid(&block)
            .unwrap_or_else(|| format!("{label}:{}:{idx}", file_stem_or_anon(file)));
        let href = if idx == 0 {
            book_href.clone()
        } else {
            format!("{book_href}#{idx}")
        };
        if existing.contains_key(&href) {
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
        let summary = fetch(FetchOptions {
            db_path: db_path.clone(),
            db: None,
            input_path: dir.path().to_path_buf(),
            account_id_override: None,
            progress: Progress::default(),
            control: ExtractControl::default(),
        })
        .await
        .unwrap();
        assert_eq!(summary.contacts_new, 2);
        assert_eq!(summary.addressbooks, 1);

        let db = RawDb::open(&db_path).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 2);
    }
}
