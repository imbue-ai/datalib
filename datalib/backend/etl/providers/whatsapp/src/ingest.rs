//! Decrypt → mirror → register media, for a single WhatsApp backup directory.
//!
//! [`fetch`] decrypts `Databases/msgstore.db.crypt15` to a tempfile, hands
//! that file to the shared SQLite mirror engine (every table, drop-and-
//! refill, doltlite dedups), then walks `Media/` into the sidecar CAS and
//! the `wa_media_files` registry. The caller commits.
//!
//! No plaintext ever reaches a user-visible path — the decrypted msgstore is a
//! `NamedTempFile` dropped at the end, and media are read in place from
//! `backup_dir/Media/`, which WhatsApp already stores in the clear.

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use datalib_etl::blob_cas::{self, BlobCas, CasInsert};
use datalib_etl::doltlite_raw;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};
use datalib_whatsapp_backup::decrypt_file;

use crate::schema_raw::{ALL_DDL, WA_MEDIA_FILES};

/// The plaintext attachment tree inside a WhatsApp backup, and the prefix
/// msgstore puts on every `message_media.file_path`.
const MEDIA_DIR: &str = "Media";

/// How many bytes of media may sit in memory before a CAS flush.
const PUT_BATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct IngestSummary {
    pub mirror: MirrorStats,
    pub media_files: u64,
}

impl IngestSummary {
    pub fn summary(&self) -> String {
        format!("{} media_files={}", self.mirror.summary(), self.media_files)
    }
}

/// The mirror engine's knobs, minus the source path (that is the tempfile
/// [`fetch`] decrypts into) and minus `snapshot` (a tempfile nobody else
/// has open needs none).
#[derive(Debug, Clone, Default)]
pub struct MirrorKnobs {
    pub include_tables: Vec<String>,
    pub exclude_tables: Vec<String>,
    pub exclude_columns: Vec<String>,
    pub gc: bool,
}

impl MirrorKnobs {
    pub fn everything() -> Self {
        Self {
            include_tables: vec!["*".to_string()],
            ..Default::default()
        }
    }
}

/// Thin wrapper over the doltlite raw-store pool, mirroring the
/// `RawDb` pattern every other provider uses. Lets the sync
/// orchestrator open the pool once at the start of an ingest run
/// (so SIGINT can flush in-flight stores) and pass the same handle
/// into `fetch`.
#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// Media bytes. Opened with the handle rather than from a path
    /// further down, so there is one opener per store and `close_all`
    /// reaches it.
    cas: BlobCas,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = doltlite_raw::open(db_path, ALL_DDL).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }
}

/// Standalone: open, fetch, commit, close. The DAG step goes through
/// [`fetch`] with a store the orchestrator opened.
pub async fn ingest(
    backup_dir: &Path,
    root_key: &[u8; 32],
    target_db_path: &Path,
    cache: &FingerprintCache,
) -> Result<IngestSummary> {
    let db = RawDb::open(target_db_path).await?;
    let out = fetch(
        backup_dir,
        root_key,
        &db,
        cache,
        &MirrorKnobs::everything(),
        &Progress::noop(),
    )
    .await;
    if out.is_ok() {
        doltlite_raw::commit_run(db.pool(), "whatsapp ingest").await?;
    }
    db.close().await;
    out
}

pub async fn fetch(
    backup_dir: &Path,
    root_key: &[u8; 32],
    db: &RawDb,
    cache: &FingerprintCache,
    knobs: &MirrorKnobs,
    progress: &Progress,
) -> Result<IngestSummary> {
    let crypt_path = backup_dir.join("Databases").join("msgstore.db.crypt15");
    tracing::info!(crypt_path = %crypt_path.display(), "whatsapp::ingest start");

    let plaintext = decrypt_file(&crypt_path, root_key)
        .with_context(|| format!("decrypt {}", crypt_path.display()))?;
    tracing::info!(
        decrypted_bytes = plaintext.len(),
        "whatsapp::ingest: msgstore decrypted"
    );
    let tmp = tempfile::Builder::new()
        .prefix("wa-msgstore-")
        .suffix(".db")
        .tempfile()
        .context("create tempfile for decrypted msgstore")?;
    std::fs::write(tmp.path(), &plaintext).context("write decrypted msgstore to tempfile")?;
    drop(plaintext);

    let options = MirrorOptions {
        source_path: tmp.path().to_path_buf(),
        snapshot: false,
        include_tables: knobs.include_tables.clone(),
        exclude_tables: knobs.exclude_tables.clone(),
        exclude_columns: knobs.exclude_columns.clone(),
        stable_key_columns: Vec::new(),
        primary_keys: Default::default(),
        gc: knobs.gc,
        sidecar_tables: vec![WA_MEDIA_FILES.to_string()],
    };
    let mut summary = IngestSummary {
        mirror: mirror::run(db.pool(), &options, progress).await?,
        media_files: 0,
    };
    drop(tmp);

    let media_root = backup_dir.join(MEDIA_DIR);
    if media_root.is_dir() {
        mirror_media_files(db.pool(), db.cas(), &media_root, cache, &mut summary).await?;
    } else {
        tracing::info!(
            media_root = %media_root.display(),
            "whatsapp::ingest: no Media/ dir; skipping media-file registry"
        );
    }

    tracing::info!(summary = %summary.summary(), "whatsapp::ingest done");
    Ok(summary)
}

/// Register every media file in `wa_media_files` and make sure its bytes are
/// in the sibling blob CAS, keyed by blake3. Render joins
/// `message_media.file_path` → `wa_media_files.relative_path` and uses that
/// row's blake3 as the CAS key.
///
/// **`relative_path` is relative to the backup root, not to `Media/`.** That is
/// what makes the join above match: msgstore spells a message's attachment
/// `Media/WhatsApp Images/IMG-….jpg`, so a registry keyed on the path below
/// `Media/` matches nothing and every attachment renders as "not yet fetched"
/// while its bytes sit in the CAS unreferenced.
///
/// One scan of `Media/` produces path, size and hash without opening a file,
/// because the host fingerprint cache vouches for anything whose stat has not
/// moved; bytes are read only for hashes the CAS lacks, in bounded batches.
/// A real `Media/` folder is gigabytes.
///
/// Dot-prefixed entries (`.Thumbs`, `.Shared`, `.trash`, `.wamocache`) are
/// WhatsApp's own scratch state, not message media.
async fn mirror_media_files(
    dst: &SqlitePool,
    cas: &BlobCas,
    media_root: &Path,
    cache: &FingerprintCache,
    summary: &mut IngestSummary,
) -> Result<()> {
    let scan = fsscan::scan(
        cache,
        media_root,
        &fsscan::ScanOptions {
            ignore: vec![".*".to_string()],
            ..Default::default()
        },
        |_| true,
    )
    .await?;
    for e in &scan.errors {
        tracing::warn!(
            event = "wa_media_walk_error",
            path = %e.path.display(),
            error = %e.error,
        );
    }

    // Drop-and-refill, like the mirrored tables: a byte-identical refill
    // is not a change to doltlite, and a file gone from `Media/` goes
    // from the registry.
    let mut tx = dst.begin().await.context("begin wa_media_files tx")?;
    sqlx::query("DELETE FROM wa_media_files")
        .execute(&mut *tx)
        .await
        .context("clear wa_media_files")?;
    for f in &scan.files {
        sqlx::query(
            "INSERT OR IGNORE INTO wa_media_files \
                (blake3, relative_path, size_bytes, mime_type) VALUES (?, ?, ?, ?)",
        )
        .bind(fsscan::hex(&f.blake3))
        .bind(format!("{MEDIA_DIR}/{}", f.rel))
        .bind(f.size)
        .bind(mime_from_ext(&f.path))
        .execute(&mut *tx)
        .await
        .context("insert wa_media_files")?;
    }
    tx.commit().await.context("commit wa_media_files tx")?;
    summary.media_files = scan.files.len() as u64;

    // Bytes: only for hashes the CAS does not already hold. Through the
    // caller's handle: opening one here would be a second opener for a
    // store the handle already owns.
    let known: HashSet<String> = sqlx::query_scalar("SELECT blake3 FROM cas_objects")
        .fetch_all(cas.pool())
        .await
        .context("read cas_objects blake3s")?
        .into_iter()
        .collect();

    let mut staged: HashSet<String> = HashSet::new();
    let mut pending: Vec<(String, Vec<u8>, Option<String>)> = Vec::new();
    let mut pending_bytes: u64 = 0;
    for f in &scan.files {
        let hex = fsscan::hex(&f.blake3);
        if known.contains(&hex) || !staged.insert(hex.clone()) {
            continue;
        }
        let bytes = match std::fs::read(&f.path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    event = "wa_media_unreadable",
                    path = %f.path.display(),
                    error = %e,
                );
                continue;
            }
        };
        pending_bytes += bytes.len() as u64;
        pending.push((hex, bytes, mime_from_ext(&f.path)));
        if pending_bytes >= PUT_BATCH_BYTES {
            put_media_batch(cas, &pending).await?;
            pending.clear();
            pending_bytes = 0;
        }
    }
    put_media_batch(cas, &pending).await?;

    Ok(())
}

async fn put_media_batch(cas: &BlobCas, items: &[(String, Vec<u8>, Option<String>)]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let inserts: Vec<CasInsert<'_>> = items
        .iter()
        .map(|(h, b, ct)| CasInsert {
            blake3: h.as_str(),
            bytes: b.as_slice(),
            content_type: ct.as_deref(),
        })
        .collect();
    cas.put_many(&inserts)
        .await
        .context("blob_cas put_many wa_media_files")
}

fn mime_from_ext(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "png" => "image/png".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "mp4" => "video/mp4".to_string(),
        "mov" => "video/quicktime".to_string(),
        "webm" => "video/webm".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" | "opus" => "audio/ogg".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "amr" => "audio/amr".to_string(),
        "pdf" => "application/pdf".to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The media registry and msgstore have to spell an attachment's path
    /// the same way, because render's only link between a message and its
    /// bytes is `message_media.file_path = wa_media_files.relative_path`.
    /// Registering the path below `Media/` made that join match nothing, so
    /// every WhatsApp attachment rendered as "(not yet fetched)" while its
    /// bytes sat in the CAS.
    #[tokio::test]
    async fn media_registry_paths_are_anchored_where_msgstore_anchors_them() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let backup_dir = tmp.path().join("WhatsApp");
        let media_root = backup_dir.join(MEDIA_DIR);
        std::fs::create_dir_all(media_root.join("WhatsApp Images")).expect("mkdir");
        std::fs::write(
            media_root.join("WhatsApp Images").join("IMG-0001.jpg"),
            b"jpeg-ish bytes",
        )
        .expect("write media file");

        let db = RawDb::open(&tmp.path().join("wa_raw.doltlite_db"))
            .await
            .expect("open raw store");
        let cache = FingerprintCache::open(&tmp.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");
        let mut summary = IngestSummary::default();
        mirror_media_files(db.pool(), db.cas(), &media_root, &cache, &mut summary)
            .await
            .expect("mirror media");

        let paths: Vec<String> =
            sqlx::query_scalar("SELECT relative_path FROM wa_media_files ORDER BY relative_path")
                .fetch_all(db.pool())
                .await
                .expect("read wa_media_files");
        db.close().await;

        // Exactly the string msgstore stores in `message_media.file_path`.
        assert_eq!(paths, vec!["Media/WhatsApp Images/IMG-0001.jpg"]);
        assert_eq!(summary.media_files, 1);
    }

    /// End-to-end against the developer's real WhatsApp backup.
    /// `#[ignore]`d: needs the backup and WHATSAPP_BACKUP_DECRYPTION_KEY.
    #[tokio::test]
    #[ignore]
    async fn real_backup() {
        let key_hex = std::env::var("WHATSAPP_BACKUP_DECRYPTION_KEY")
            .expect("WHATSAPP_BACKUP_DECRYPTION_KEY env var must be set");
        let root = datalib_whatsapp_backup::decode_hex_key(&key_hex).expect("decode hex key");
        let backup_dir =
            std::path::PathBuf::from(std::env::var("WHATSAPP_BACKUP_DIR").unwrap_or_else(|_| {
                let h = std::env::var("HOME").expect("HOME set");
                format!("{h}/backups/WhatsApp")
            }));
        let tmpdir = tempfile::tempdir().expect("tmpdir");
        let target = tmpdir.path().join("wa_raw.doltlite_db");
        let cache = FingerprintCache::open(&tmpdir.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");
        let summary = ingest(&backup_dir, &root, &target, &cache)
            .await
            .expect("ingest ok");
        assert!(summary.mirror.tables > 0);
        assert!(summary.mirror.rows > 0);
    }
}
