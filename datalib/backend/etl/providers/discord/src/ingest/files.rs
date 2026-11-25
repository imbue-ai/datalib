//! The files stored messages carry, fetched into the blob CAS through
//! `datalib_etl_web::owed`. A file's link is signed for about a day, so
//! one owed from an earlier run has its message read again for a link
//! that works.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl_web::http::{latchkey_curl, HttpError, LatchkeySettings};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_etl_web::owed_files::{self, File, Got, HeldFiles};
use datalib_problems::Reason;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::api::{self, DiscordError};
use super::db::{files_of, id_of, OwedFile, RawDb};
use super::schema_raw::DiscordAttachmentRow;

pub struct FileFetcher<'a> {
    pub db: &'a RawDb,
    pub latchkey: &'a LatchkeySettings,
    pub size_limit: Option<u64>,
    /// The run's now, as Unix seconds: what a link's expiry is judged by.
    pub now_secs: i64,
    /// What each owed edge points at, read before the requests go out:
    /// the store's one connection is the writer's.
    pub owed: BTreeMap<String, OwedFile>,
    pub held: &'a HeldFiles,
    /// `attachment id → link`, from messages read again.
    pub fresh: Mutex<HashMap<String, String>>,
    pub requests: &'a AtomicU64,
    pub downloaded: &'a AtomicUsize,
}

fn text<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    v.get(key).and_then(Value::as_str)
}

#[async_trait]
impl Fetcher<File> for FileFetcher<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<File>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = self.one(&listed.key).await?;
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<File>],
    ) -> Result<()> {
        let landed =
            owed_files::store::<DiscordAttachmentRow>(self.db.cas(), tx, batch, self.held).await?;
        self.downloaded.fetch_add(landed, Ordering::Relaxed);
        Ok(())
    }

    fn weight(&self, content: &File) -> usize {
        content.weight()
    }
}

impl FileFetcher<'_> {
    async fn one(&self, key: &str) -> std::result::Result<Outcome<File>, BatchError> {
        // An edge whose message no longer carries the file points at
        // nothing.
        let Some(owed) = self.owed.get(key) else {
            return Ok(Outcome::Gone);
        };
        let Some(attachment_id) = id_of(&owed.file) else {
            return Ok(Outcome::Gone);
        };
        let file = |got| File {
            ref_id: attachment_id.to_string(),
            got,
        };
        if let Some(held) = self.held.lock().unwrap().get(attachment_id).cloned() {
            return Ok(Outcome::Got(file(Got::Held(held))));
        }
        let size = owed.file.get("size").and_then(Value::as_u64);
        if let Some(skipped) = owed_files::over_limit(size, self.size_limit) {
            return Ok(skipped);
        }
        let url = match self.working_link(owed, attachment_id).await? {
            Ok(url) => url,
            Err(outcome) => return Ok(outcome),
        };
        let Some(req) = api::file_request(&url, self.size_limit) else {
            return Ok(Outcome::Failed(
                "its link is not to Discord's own file host, so it was not followed".to_string(),
            ));
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        match latchkey_curl(&req).await {
            Ok(resp) if resp.status == 200 => Ok(Outcome::Got(file(Got::Fetched {
                bytes: resp.body,
                content_type: text(&owed.file, "content_type").map(String::from),
            }))),
            Ok(resp) => Ok(Outcome::Failed(format!("HTTP {}", resp.status))),
            Err(e @ HttpError::Interrupted { .. }) => Err(BatchError::Batch(e.into())),
            Err(e @ HttpError::TooLarge { .. }) => {
                Ok(Outcome::Skipped(Reason::OverSizeLimit, e.to_string()))
            }
            Err(e) => Ok(Outcome::Failed(e.to_string())),
        }
    }

    /// The file's link, its message read again for one that still works
    /// when the one stored has run out.
    async fn working_link(
        &self,
        owed: &OwedFile,
        attachment_id: &str,
    ) -> std::result::Result<std::result::Result<String, Outcome<File>>, BatchError> {
        let works = |url: &str| api::link_expires(url).is_none_or(|ex| ex > self.now_secs + 60);
        if let Some(url) = text(&owed.file, "url").filter(|u| works(u)) {
            return Ok(Ok(url.to_string()));
        }
        if let Some(url) = self.fresh.lock().unwrap().get(attachment_id).cloned() {
            return Ok(Ok(url));
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        let req = api::around_request(&owed.channel_id, owed.message_id);
        let page = match api::send(req, self.latchkey).await {
            Ok(page) => page,
            Err(DiscordError::Transport(e @ HttpError::Interrupted { .. })) => {
                return Err(BatchError::Batch(e.into()))
            }
            Err(e) if e.ends_the_run() => return Err(BatchError::Terminal(e.into())),
            Err(e) => return Ok(Err(Outcome::Failed(e.to_string()))),
        };
        let mut fresh = self.fresh.lock().unwrap();
        for file in page.as_array().into_iter().flatten().flat_map(files_of) {
            if let (Some(id), Some(url)) = (id_of(file), text(file, "url")) {
                fresh.insert(id.to_string(), url.to_string());
            }
        }
        Ok(fresh.get(attachment_id).cloned().ok_or(Outcome::Gone))
    }
}
