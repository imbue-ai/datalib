//! The files a channel's stored messages carry, fetched into the blob
//! CAS through `datalib_etl_web::owed`: an edge without bytes is owed, a
//! batch of them is one request's worth of downloads, and `store` puts
//! a flush's bytes into the CAS before giving each edge its `blake3`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
use tracing::debug;

use datalib_etl::progress::RunBar;
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_etl_web::owed_files::{self, File, Got, HeldFiles};

use super::api::{served, LATCHKEY_FILE_TIMEOUT};
use super::db::{OwedFile, RawDb};
use super::schema_raw::SlackAttachmentRow;

/// Files per request: how many a batch holds in memory before its bytes
/// are written.
pub const FILE_BATCH: usize = 8;

pub struct FileFetcher<'a> {
    pub db: &'a RawDb,
    pub latchkey: &'a LatchkeySettings,
    pub blob_size_limit_bytes: Option<u64>,
    /// `file_id → blake3` for every file whose bytes the CAS holds, under
    /// any message: a file's bytes never change, so one already hashed
    /// under one message is not fetched for another. Loaded once per
    /// run, grown as files land.
    pub blake3_by_file: &'a HeldFiles,
    pub bar: &'a RunBar,
    /// Files whose bytes this run fetched and stored, as against found
    /// in the CAS.
    pub downloaded: AtomicUsize,
}

#[async_trait]
impl Fetcher<File> for FileFetcher<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<File>>, BatchError> {
        let objects = async {
            let mut conn = self.db.pool().acquire().await?;
            self.objects(&mut conn, &batch).await
        };
        let objects = objects.await.map_err(BatchError::Abort)?;
        self.fetch_objects(batch, objects).await
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<File>],
    ) -> Result<()> {
        let landed =
            owed_files::store::<SlackAttachmentRow>(self.db.cas(), tx, batch, self.blake3_by_file)
                .await?;
        self.downloaded.fetch_add(landed, Ordering::Relaxed);
        Ok(())
    }
}

impl FileFetcher<'_> {
    /// The stored file object each of `batch` names. A run with requests
    /// out at once reads these before the request goes out, through the
    /// writer's transaction: the store has one connection, and a request
    /// waiting on it would stall the writer.
    pub async fn objects(
        &self,
        conn: &mut sqlx::SqliteConnection,
        batch: &[Listed],
    ) -> Result<Vec<OwedFile>> {
        let keys: Vec<&str> = batch.iter().map(|l| l.key.as_str()).collect();
        self.db.file_objects(conn, &keys).await
    }

    /// `batch`, fetched from the file `objects` its edges name. Touches
    /// no store.
    pub async fn fetch_objects(
        &self,
        batch: Vec<Listed>,
        objects: Vec<OwedFile>,
    ) -> std::result::Result<Vec<Fetched<File>>, BatchError> {
        let by_key: HashMap<&str, &OwedFile> =
            objects.iter().map(|o| (o.key.as_str(), o)).collect();
        // Files this batch fetched, so a file twice in it is fetched
        // once; later batches learn their hashes from `store`.
        let mut fresh: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = self
                .one(by_key.get(listed.key.as_str()).copied(), &mut fresh)
                .await
                .map_err(BatchError::Batch)?;
            self.bar.did(1);
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    /// What one owed edge comes to. `Err` only when the run was told to
    /// stop; a download that fails is the record's own outcome.
    async fn one(
        &self,
        owed: Option<&OwedFile>,
        fresh: &mut HashSet<String>,
    ) -> Result<Outcome<File>> {
        // An edge whose message no longer carries a file Slack serves
        // points at nothing to fetch.
        let Some((owed, (file_id, url))) = owed.and_then(|o| Some((o, served(&o.file)?))) else {
            return Ok(Outcome::Gone);
        };
        let known = self.blake3_by_file.lock().unwrap().get(file_id).cloned();
        if let Some(blake3) = known {
            return Ok(Outcome::Got(File {
                ref_id: file_id.to_string(),
                got: Got::Held(blake3),
            }));
        }
        if fresh.contains(file_id) {
            return Ok(Outcome::Got(File {
                ref_id: file_id.to_string(),
                got: Got::FetchedAbove,
            }));
        }
        let size = owed.file.get("size").and_then(Value::as_u64);
        if let Some(skipped) = owed_files::over_limit(size, self.blob_size_limit_bytes) {
            return Ok(skipped);
        }
        let req = HttpRequest::get(HttpService::Slack, url)
            .latchkey(self.latchkey.clone())
            .timeout(LATCHKEY_FILE_TIMEOUT);
        let resp = match latchkey_curl(&req).await {
            Ok(resp) if resp.status == 200 => resp,
            Err(e @ HttpError::Interrupted { .. }) => return Err(e.into()),
            failed => {
                let failure = match failed {
                    // What a stored URL answers for a file that is gone, or
                    // that the credential cannot see: a redirect to a 404.
                    Ok(resp) if resp.status == 302 => {
                        "HTTP 302: the file is gone or this account cannot see it".to_string()
                    }
                    Ok(resp) => format!("HTTP {}", resp.status),
                    Err(e) => e.to_string(),
                };
                return Ok(Outcome::Failed(failure));
            }
        };
        let bytes = resp.body;
        let len = bytes.len() as u64;
        debug!(
            event = "slack_media_downloaded",
            file_id = file_id,
            bytes = len,
            "downloaded one file"
        );
        fresh.insert(file_id.to_string());
        Ok(Outcome::Got(File {
            ref_id: file_id.to_string(),
            got: Got::Fetched {
                bytes,
                content_type: owed
                    .file
                    .get("mimetype")
                    .and_then(Value::as_str)
                    .map(String::from),
            },
        }))
    }
}
