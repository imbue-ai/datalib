//! The files a channel's stored messages carry, fetched into the blob
//! CAS through `datalib_etl_web::owed`: an edge without bytes is owed, a
//! batch of them is one request's worth of downloads, and `store` puts
//! a flush's bytes into the CAS before giving each edge its `blake3`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
use tracing::debug;

use datalib_etl::blob_cas::{blake3_hex, CasInsert};
use datalib_etl::events;
use datalib_etl::progress::RunBar;
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};

use super::api::{served, LATCHKEY_FILE_TIMEOUT};
use super::db::{OwedFile, RawDb};

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
    pub blake3_by_file: &'a Mutex<HashMap<String, String>>,
    pub bar: &'a RunBar,
    /// Files whose bytes this run fetched, as against found in the CAS.
    pub downloaded: AtomicUsize,
}

/// What one edge's fetch came to: the hash its edge gets, with the
/// bytes behind it when this run fetched them.
pub struct File {
    file_id: String,
    blake3: String,
    /// `None` when the CAS holds the bytes already.
    fetched: Option<Bytes>,
}

struct Bytes {
    bytes: Vec<u8>,
    mime: Option<String>,
}

#[async_trait]
impl Fetcher<File> for FileFetcher<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<File>>, BatchError> {
        let keys: Vec<&str> = batch.iter().map(|l| l.key.as_str()).collect();
        let objects = self
            .db
            .file_objects(&keys)
            .await
            .map_err(BatchError::Abort)?;
        let by_key: HashMap<&str, &OwedFile> =
            objects.iter().map(|o| (o.key.as_str(), o)).collect();
        // Hashes this batch computed, so a file twice in it is fetched
        // once; later batches learn them from `store`.
        let mut fresh: HashMap<String, String> = HashMap::new();
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

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<File>],
    ) -> Result<()> {
        let files = batch.iter().filter_map(|f| match &f.outcome {
            Outcome::Got(file) | Outcome::Unusable(file, ..) => Some(file),
            _ => None,
        });
        let inserts: Vec<CasInsert<'_>> = files
            .clone()
            .filter_map(|file| {
                let fetched = file.fetched.as_ref()?;
                Some(CasInsert {
                    blake3: &file.blake3,
                    bytes: &fetched.bytes,
                    content_type: fetched.mime.as_deref(),
                })
            })
            .collect();
        self.db.cas().put_many(&inserts).await?;
        for f in batch {
            match &f.outcome {
                Outcome::Got(file) | Outcome::Unusable(file, ..) => {
                    sqlx::query("UPDATE slack_attachments SET blake3 = ? WHERE id = ?")
                        .bind(&file.blake3)
                        .bind(&f.listed.key)
                        .execute(&mut **tx)
                        .await?;
                }
                Outcome::Gone => {
                    sqlx::query("DELETE FROM slack_attachments WHERE id = ?")
                        .bind(&f.listed.key)
                        .execute(&mut **tx)
                        .await?;
                }
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        self.blake3_by_file
            .lock()
            .unwrap()
            .extend(files.map(|file| (file.file_id.clone(), file.blake3.clone())));
        Ok(())
    }
}

impl FileFetcher<'_> {
    /// What one owed edge comes to. `Err` only when the run was told to
    /// stop; a download that fails is the record's own outcome.
    async fn one(
        &self,
        owed: Option<&OwedFile>,
        fresh: &mut HashMap<String, String>,
    ) -> Result<Outcome<File>> {
        // An edge whose message no longer carries a file Slack serves
        // points at nothing to fetch.
        let Some((owed, (file_id, url))) = owed.and_then(|o| Some((o, served(&o.file)?))) else {
            return Ok(Outcome::Gone);
        };
        let known = self
            .blake3_by_file
            .lock()
            .unwrap()
            .get(file_id)
            .cloned()
            .or_else(|| fresh.get(file_id).cloned());
        if let Some(blake3) = known {
            return Ok(Outcome::Got(File {
                file_id: file_id.to_string(),
                blake3,
                fetched: None,
            }));
        }
        if let (Some(limit), Some(size)) = (
            self.blob_size_limit_bytes,
            owed.file.get("size").and_then(Value::as_u64),
        ) {
            if size > limit {
                debug!(
                    event = "slack_media_too_large",
                    file_id = file_id,
                    size = size,
                    limit = limit,
                    "a file is over the size limit; skipped it"
                );
                return Ok(Outcome::Skipped(
                    datalib_problems::Reason::OverSizeLimit,
                    format!("size {size} > limit {limit}"),
                ));
            }
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
        let blake3 = blake3_hex(&bytes);
        events::item_fetched(url, len, resp.duration_ms);
        debug!(
            event = "slack_media_downloaded",
            file_id = file_id,
            bytes = len,
            "downloaded one file"
        );
        fresh.insert(file_id.to_string(), blake3.clone());
        self.downloaded.fetch_add(1, Ordering::Relaxed);
        Ok(Outcome::Got(File {
            file_id: file_id.to_string(),
            blake3,
            fetched: Some(Bytes {
                bytes,
                mime: owed
                    .file
                    .get("mimetype")
                    .and_then(Value::as_str)
                    .map(String::from),
            }),
        }))
    }
}
