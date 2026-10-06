//! Decrypt → mirror → register media, for a single WhatsApp backup directory.
//!
//! [`fetch`] decrypts `Databases/msgstore.db.crypt15` to a tempfile, hands
//! that file to the shared SQLite mirror engine (every table, drop-and-
//! refill, doltlite dedups), copies `wa.db`'s contacts into
//! `wa_db_contacts`, then walks `Media/` into the sidecar CAS and the
//! `wa_media_files` registry. The caller commits.
//!
//! No plaintext ever reaches a user-visible path — the decrypted msgstore is a
//! `NamedTempFile` dropped at the end, and media are read in place from
//! `backup_dir/Media/`, which WhatsApp already stores in the clear.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool};
use sqlx::Connection;
use std::str::FromStr;

use datalib_etl::blob_cas::{BlobCas, CasInsert};
use datalib_etl::doltlite_raw;
use datalib_etl::download_problems::RecordProblem;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};
use datalib_whatsapp_backup::decrypt_file;

use crate::schema_raw::{ALL_DDL, WA_DB_CONTACTS, WA_MEDIA_FILES};

/// The plaintext attachment tree inside a WhatsApp backup, and the prefix
/// msgstore puts on every `message_media.file_path`.
const MEDIA_DIR: &str = "Media";

/// Where WhatsApp backs up `wa.db`, the database that holds the names.
const WA_DB_BACKUP: &str = "Backups/wa.db.crypt15";

/// How many bytes of media may sit in memory before a CAS flush.
const PUT_BATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct IngestSummary {
    pub mirror: MirrorStats,
    pub media_files: u64,
    /// `wa.db` contact rows copied; `None` when the backup has no `wa.db`.
    pub contacts: Option<u64>,
}

impl IngestSummary {
    pub fn summary(&self) -> String {
        let contacts = self
            .contacts
            .map_or_else(|| "none (no wa.db)".to_string(), |n| n.to_string());
        format!(
            "{} media_files={} contacts={contacts}",
            self.mirror.summary(),
            self.media_files
        )
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

datalib_etl::raw_db! {
    /// Opened once by the orchestrator at the start of an ingest run (so
    /// SIGINT can flush in-flight stores) and passed into `fetch`.
    pub RawDb: CasEntityStore, ALL_DDL
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
    // The mirror has no stop to honour, so it always reads the whole backup.
    let never_stops = StopFlag::new();
    run_problems::collecting(db.pool(), &never_stops, |found| {
        read_backup(backup_dir, root_key, db, cache, knobs, progress, found)
    })
    .await
}

async fn read_backup(
    backup_dir: &Path,
    root_key: &[u8; 32],
    db: &RawDb,
    cache: &FingerprintCache,
    knobs: &MirrorKnobs,
    progress: &Progress,
    found: RunProblems,
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
        snapshot: false,
        include_tables: knobs.include_tables.clone(),
        exclude_tables: knobs.exclude_tables.clone(),
        exclude_columns: knobs.exclude_columns.clone(),
        gc: knobs.gc,
        sidecar_tables: vec![WA_MEDIA_FILES.to_string(), WA_DB_CONTACTS.to_string()],
        ..MirrorOptions::new(tmp.path())
    };
    let mut summary = IngestSummary {
        mirror: mirror::run(db.pool(), &options, progress).await?,
        media_files: 0,
        contacts: None,
    };
    drop(tmp);

    mirror_beside_msgstore(backup_dir, root_key, db, cache, &mut summary, &found).await?;
    tracing::info!(summary = %summary.summary(), "whatsapp::ingest done");
    Ok(summary)
}

/// What the backup holds beside msgstore: `wa.db`'s contacts and `Media/`.
/// One that will not read is a `problems` row, and what an earlier run
/// stored of it stays; msgstore itself is mirrored by then.
async fn mirror_beside_msgstore(
    backup_dir: &Path,
    root_key: &[u8; 32],
    db: &RawDb,
    cache: &FingerprintCache,
    summary: &mut IngestSummary,
    found: &RunProblems,
) -> Result<()> {
    let wa_db = backup_dir.join(WA_DB_BACKUP);
    if wa_db.is_file() {
        match read_wa_db_contacts(&wa_db, root_key).await {
            Ok(rows) => summary.contacts = Some(store_contacts(db.pool(), &rows).await?),
            Err(e) => found.phase("wa.db contacts", format!("{e:#}")),
        }
    } else {
        // Like a missing `Media/`: a copy of the backup made without
        // `Backups/` says nothing about the contacts, so the ones stored
        // stay.
        tracing::info!(
            wa_db = %wa_db.display(),
            "whatsapp::ingest: no wa.db backup; keeping the stored contacts"
        );
    }

    let media_root = backup_dir.join(MEDIA_DIR);
    if media_root.is_dir() {
        mirror_media_files(db.pool(), db.cas(), &media_root, cache, summary, found).await?;
    } else {
        tracing::info!(
            media_root = %media_root.display(),
            "whatsapp::ingest: no Media/ dir; skipping media-file registry"
        );
    }
    Ok(())
}

/// Decrypt `wa.db` to a tempfile and read its `wa_contacts`, every column
/// as JSON (see [`crate::schema_raw::WA_DB_CONTACTS_DDL`]), through one
/// plain connection closed before returning.
async fn read_wa_db_contacts(
    crypt_path: &Path,
    root_key: &[u8; 32],
) -> Result<Vec<(String, String)>> {
    let plaintext = decrypt_file(crypt_path, root_key)
        .with_context(|| format!("decrypt {}", crypt_path.display()))?;
    let tmp = tempfile::Builder::new()
        .prefix("wa-db-")
        .suffix(".db")
        .tempfile()
        .context("create tempfile for decrypted wa.db")?;
    std::fs::write(tmp.path(), &plaintext).context("write decrypted wa.db to tempfile")?;
    drop(plaintext);

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", tmp.path().display()))
        .context("sqlite uri for decrypted wa.db")?
        .read_only(true)
        .create_if_missing(false);
    let mut src = SqliteConnection::connect_with(&opts)
        .await
        .context("open decrypted wa.db")?;
    let rows = read_contacts(&mut src).await;
    let _ = src.close().await;
    rows
}

/// Drop-and-refill `wa_db_contacts`. Returns the rows copied.
async fn store_contacts(dst: &SqlitePool, rows: &[(String, String)]) -> Result<u64> {
    let mut tx = dst.begin().await.context("begin wa_db_contacts tx")?;
    sqlx::query("DELETE FROM wa_db_contacts")
        .execute(&mut *tx)
        .await
        .context("clear wa_db_contacts")?;
    for (jid, contact_rows) in rows {
        sqlx::query("INSERT INTO wa_db_contacts (jid, rows) VALUES (?, ?)")
            .bind(jid)
            .bind(contact_rows)
            .execute(&mut *tx)
            .await
            .context("insert wa_db_contacts")?;
    }
    tx.commit().await.context("commit wa_db_contacts tx")?;
    Ok(rows.len() as u64)
}

/// `(jid, [every column of each of its rows as a JSON object])`, one per
/// jid. A BLOB column goes in as hex: JSON cannot hold one.
async fn read_contacts(src: &mut SqliteConnection) -> Result<Vec<(String, String)>> {
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('wa_contacts')")
            .fetch_all(&mut *src)
            .await
            .context("wa_contacts columns")?;
    if columns.is_empty() {
        anyhow::bail!("wa.db has no wa_contacts table");
    }
    let pairs: Vec<String> = columns
        .iter()
        .map(|c| {
            let lit = c.replace('\'', "''");
            let ident = c.replace('"', "\"\"");
            format!(
                "'{lit}', CASE WHEN typeof(\"{ident}\") = 'blob' THEN hex(\"{ident}\") ELSE \"{ident}\" END"
            )
        })
        .collect();
    let sql = format!(
        "SELECT jid, json_group_array(json(obj)) FROM \
           (SELECT jid, json_object({}) AS obj FROM wa_contacts \
             WHERE jid IS NOT NULL ORDER BY _id) \
         GROUP BY jid ORDER BY jid",
        pairs.join(", ")
    );
    // Audited: the column names come from the file's own
    // `pragma_table_info`, quoted as identifiers and as string literals.
    let rows = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *src)
        .await
        .context("read wa_contacts")?;
    Ok(rows)
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
    found: &RunProblems,
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
    found.extend(scan.walk_problems_as("media"));

    // Drop-and-refill, like the mirrored tables: a byte-identical refill
    // is not a change to doltlite, and a file gone from `Media/` goes
    // from the registry. Not after a walk that could not read part of the
    // tree: the files under it are not gone.
    let mut tx = dst.begin().await.context("begin wa_media_files tx")?;
    if scan.errors.is_empty() {
        sqlx::query("DELETE FROM wa_media_files")
            .execute(&mut *tx)
            .await
            .context("clear wa_media_files")?;
    }
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

    // `(hash, problem)`: a copy of the same bytes elsewhere may still read.
    let mut unreadable: Vec<(String, RecordProblem)> = Vec::new();
    let mut staged: HashSet<String> = HashSet::new();
    let mut pending: Vec<(String, Vec<u8>, Option<String>)> = Vec::new();
    let mut pending_bytes: u64 = 0;
    for f in &scan.files {
        let hex = fsscan::hex(&f.blake3);
        if known.contains(&hex) || staged.contains(&hex) {
            continue;
        }
        let bytes = match std::fs::read(&f.path) {
            Ok(b) => b,
            Err(e) => {
                let problem = RecordProblem::new(
                    WA_MEDIA_FILES,
                    &format!("{MEDIA_DIR}/{}", f.rel),
                    e.to_string(),
                );
                unreadable.push((hex, problem));
                continue;
            }
        };
        staged.insert(hex.clone());
        pending_bytes += bytes.len() as u64;
        pending.push((hex, bytes, mime_from_ext(&f.path)));
        if pending_bytes >= PUT_BATCH_BYTES {
            put_media_batch(cas, &pending).await?;
            pending.clear();
            pending_bytes = 0;
        }
    }
    put_media_batch(cas, &pending).await?;

    found.records_failed(
        unreadable
            .into_iter()
            .filter(|(hex, _)| !staged.contains(hex))
            .map(|(_, problem)| problem),
    );
    // Every run reads again whatever the CAS still lacks, so a row stands
    // only on a file under a folder this walk could not list.
    let unseen = scan.unseen();
    found.records_tried_all_but(WA_MEDIA_FILES, move |id| {
        id.strip_prefix(MEDIA_DIR)
            .and_then(|rel| rel.strip_prefix('/'))
            .is_some_and(&unseen)
    });
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
        let found = RunProblems::unwritten();
        mirror_media_files(
            db.pool(),
            db.cas(),
            &media_root,
            &cache,
            &mut summary,
            &found,
        )
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

    /// The half of a fetch after msgstore, its problems written as a
    /// fetch writes them.
    async fn mirror_beside_msgstore(
        backup_dir: &Path,
        root_key: &[u8; 32],
        db: &RawDb,
        cache: &FingerprintCache,
        summary: &mut IngestSummary,
    ) -> Result<()> {
        run_problems::collecting(db.pool(), &StopFlag::new(), |found| async move {
            super::mirror_beside_msgstore(backup_dir, root_key, db, cache, summary, &found).await
        })
        .await
    }

    async fn problems(db: &RawDb) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .expect("read problems")
    }

    async fn registry(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT relative_path FROM wa_media_files ORDER BY relative_path")
            .fetch_all(db.pool())
            .await
            .expect("read wa_media_files")
    }

    /// `path` set to `mode`; `false` when that left it readable, as it does
    /// for root in a container.
    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        std::fs::read_dir(path).is_err() && std::fs::read(path).is_err()
    }

    fn write_media(backup_dir: &Path, rel: &str, bytes: &[u8]) {
        let path = backup_dir.join(MEDIA_DIR).join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write media file");
    }

    /// A walk that could not list part of `Media/` still dropped the whole
    /// registry and refilled it from what it saw, so every file under the
    /// unreadable folder left the registry, and only a log line said why.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_media_folder_that_will_not_list_deletes_nothing() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let backup_dir = tmp.path().join("WhatsApp");
        write_media(&backup_dir, "WhatsApp Images/IMG-1701.jpg", b"enterprise");
        write_media(
            &backup_dir,
            "WhatsApp Video/VID-1701.mp4",
            b"saucer separation",
        );
        let db = RawDb::open(&tmp.path().join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        let cache = FingerprintCache::open(&tmp.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");
        let run = || async {
            mirror_beside_msgstore(
                &backup_dir,
                &[0u8; 32],
                &db,
                &cache,
                &mut Default::default(),
            )
            .await
            .expect("ingest beside msgstore");
        };
        run().await;
        let both = registry(&db).await;
        assert_eq!(both.len(), 2);

        let video = backup_dir.join(MEDIA_DIR).join("WhatsApp Video");
        if set_mode(&video, 0o000) {
            run().await;
            assert_eq!(
                registry(&db).await,
                both,
                "a file it could not see is not gone"
            );
            assert_eq!(
                problems(&db).await,
                vec![("listing:media".into(), "error".into())]
            );
        }
        set_mode(&video, 0o755);
        run().await;
        assert!(problems(&db).await.is_empty());
        db.close().await;
    }

    /// A media file whose bytes would not read was a log line, and the
    /// message that names it rendered as "not yet fetched" with nothing
    /// to say why.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_media_file_that_will_not_read_is_a_problem_until_it_does() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let backup_dir = tmp.path().join("WhatsApp");
        write_media(&backup_dir, "WhatsApp Images/IMG-1701.jpg", b"enterprise");
        let cache = FingerprintCache::open(&tmp.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");
        // A first store, so the host cache vouches for the file's hash and
        // the next walk does not open it.
        // Its own folder: a store's blob CAS is the folder's.
        let first = RawDb::open(&tmp.path().join("first").join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        mirror_beside_msgstore(
            &backup_dir,
            &[0u8; 32],
            &first,
            &cache,
            &mut Default::default(),
        )
        .await
        .expect("ingest beside msgstore");
        first.close().await;

        let db = RawDb::open(&tmp.path().join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        let file = backup_dir
            .join(MEDIA_DIR)
            .join("WhatsApp Images/IMG-1701.jpg");
        if set_mode(&file, 0o000) {
            mirror_beside_msgstore(
                &backup_dir,
                &[0u8; 32],
                &db,
                &cache,
                &mut Default::default(),
            )
            .await
            .expect("ingest beside msgstore");
            assert_eq!(
                problems(&db).await,
                vec![(
                    "record:wa_media_files:Media/WhatsApp Images/IMG-1701.jpg".into(),
                    "error".into()
                )]
            );
        }
        set_mode(&file, 0o644);
        mirror_beside_msgstore(
            &backup_dir,
            &[0u8; 32],
            &db,
            &cache,
            &mut Default::default(),
        )
        .await
        .expect("ingest beside msgstore");
        assert!(problems(&db).await.is_empty());
        db.close().await;
    }

    /// One unreadable copy of a file kept a readable copy of the same bytes
    /// from being read, so the bytes never reached the CAS.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_copy_does_not_block_a_readable_one() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let backup_dir = tmp.path().join("WhatsApp");
        write_media(&backup_dir, "WhatsApp Images/IMG-0001.jpg", b"tribble");
        write_media(&backup_dir, "WhatsApp Images/IMG-0002.jpg", b"tribble");
        let cache = FingerprintCache::open(&tmp.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");
        // A first store, so the host cache vouches for both hashes.
        let first = RawDb::open(&tmp.path().join("first").join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        mirror_beside_msgstore(
            &backup_dir,
            &[0u8; 32],
            &first,
            &cache,
            &mut Default::default(),
        )
        .await
        .expect("ingest beside msgstore");
        first.close().await;

        let db = RawDb::open(&tmp.path().join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        let file = backup_dir
            .join(MEDIA_DIR)
            .join("WhatsApp Images/IMG-0001.jpg");
        if set_mode(&file, 0o000) {
            mirror_beside_msgstore(
                &backup_dir,
                &[0u8; 32],
                &db,
                &cache,
                &mut Default::default(),
            )
            .await
            .expect("ingest beside msgstore");
            assert!(problems(&db).await.is_empty());
            let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM cas_objects")
                .fetch_one(db.cas().pool())
                .await
                .expect("count cas");
            assert_eq!(stored, 1);
        }
        set_mode(&file, 0o644);
        db.close().await;
    }

    /// A `wa.db` that would not decrypt failed the step after msgstore
    /// had been mirrored. It is a row now, and the stored names stay.
    #[tokio::test]
    async fn a_wa_db_that_will_not_read_keeps_the_stored_contacts() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let backup_dir = tmp.path().join("WhatsApp");
        let crypt = backup_dir.join(WA_DB_BACKUP);
        std::fs::create_dir_all(crypt.parent().expect("parent")).expect("mkdir");
        std::fs::write(&crypt, b"not a crypt15 file, Captain").expect("write wa.db");
        let db = RawDb::open(&tmp.path().join("wa.doltlite_db"))
            .await
            .expect("open raw store");
        store_contacts(
            db.pool(),
            &[(
                "1701@s.whatsapp.net".into(),
                "[{\"display_name\":\"Data\"}]".into(),
            )],
        )
        .await
        .expect("store a contact");
        let cache = FingerprintCache::open(&tmp.path().join("fp.sqlite"))
            .await
            .expect("open fingerprint cache");

        let mut summary = IngestSummary::default();
        mirror_beside_msgstore(&backup_dir, &[0u8; 32], &db, &cache, &mut summary)
            .await
            .expect("a wa.db that will not read does not fail the run");
        assert_eq!(summary.contacts, None);
        let kept: Vec<String> = sqlx::query_scalar("SELECT jid FROM wa_db_contacts")
            .fetch_all(db.pool())
            .await
            .expect("read contacts");
        assert_eq!(kept, vec!["1701@s.whatsapp.net".to_string()]);
        assert_eq!(
            problems(&db).await,
            vec![("phase:wa.db contacts".into(), "error".into())]
        );
        db.close().await;
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
        assert_eq!(
            summary.contacts.is_some(),
            backup_dir.join(WA_DB_BACKUP).is_file(),
            "a backup with a wa.db has its contacts copied: {}",
            summary.summary()
        );
    }
}
