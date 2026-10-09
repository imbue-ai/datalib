//! What each loop fetches and how a flush of it is stored, one
//! [`Fetcher`] per kind: a conversation's detail, an attachment's bytes.
//! Each is one request per record; the loop (`datalib_etl_web::owed`) owns
//! the stop, the failure budget, the flush and what each outcome means
//! for the record's sidecar.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::blob_cas::{CasEdgeRow as _, CasInsert};
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_problems::Reason;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
use tokio::time::sleep;

use super::api::ChatGPTError;
use super::db::Conversation;
use super::schema_raw::ConversationAttachmentRow;
use super::{parse_conversation, Ctx, ATTACH_FILE_TIMEOUT};

/// What a refused request means for the record, or for the loop: the
/// run ends on a rate limit, a stop leaves the record to the next run,
/// anything else is a failure. A 404 is one too: the listing just named
/// the conversation, and only a complete listing that leaves it out
/// says it is gone.
fn not_fetched<T>(ctx: &Ctx<'_>, e: ChatGPTError) -> Result<Outcome<T>, BatchError> {
    if ctx.stop().requested() {
        return Err(BatchError::Batch(e.into()));
    }
    match e {
        ChatGPTError::RateLimited { .. } => Err(BatchError::Terminal(e.into())),
        ChatGPTError::Permanent(msg) => Ok(Outcome::Failed(msg)),
    }
}

// ── conversations ──────────────────────────────────────────────────────

pub(crate) struct Conversations<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Conversation> for Conversations<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Conversation>>, BatchError> {
        let ctx = self.0;
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            ctx.opts.progress.set_message(&listed.key);
            let outcome = match ctx.client.get_conversation(&listed.key).await {
                Ok(full) => match parse_conversation(&listed.key, &full) {
                    Ok(c) => Outcome::Got(c),
                    Err(e) => Outcome::Failed(format!("{e:#}")),
                },
                Err(e) => not_fetched(ctx, e)?,
            };
            ctx.opts.progress.inc(1);
            out.push(Fetched { listed, outcome });
            if ctx.opts.sleep_between > Duration::ZERO {
                sleep(ctx.opts.sleep_between).await;
            }
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Conversation>],
    ) -> Result<()> {
        for f in batch {
            match &f.outcome {
                Outcome::Got(c) | Outcome::Unusable(c, ..) => {
                    self.0.db.store_conversation(tx, c).await?;
                    self.0.remember_files(c);
                }
                Outcome::Gone => self.0.db.forget_conversation(tx, &f.listed.key).await?,
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        Ok(())
    }

    fn weight(&self, c: &Conversation) -> usize {
        c.payload.len()
    }
}

// ── attachments ────────────────────────────────────────────────────────

/// What one edge's fetch came to.
pub enum Blob {
    /// The CAS holds the bytes already, under another conversation, by
    /// this hash.
    Held(String),
    /// Bytes this run fetched, and their type. The CAS names them.
    Fetched(Vec<u8>, Option<String>),
    /// A file chatgpt.com no longer has.
    Missing,
}

pub(crate) struct Attachments<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Blob> for Attachments<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Blob>>, BatchError> {
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
        batch: &[Fetched<Blob>],
    ) -> Result<()> {
        let inserts: Vec<CasInsert<'_, &str>> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(Blob::Fetched(bytes, content_type))
                | Outcome::Unusable(Blob::Fetched(bytes, content_type), ..) => Some(CasInsert {
                    id: f.listed.key.as_str(),
                    bytes,
                    content_type: content_type.as_deref(),
                }),
                _ => None,
            })
            .collect();
        let stored = self.0.db.cas().put_many(inserts).await?;
        for f in batch {
            let blake3 = match &f.outcome {
                Outcome::Got(Blob::Held(held)) | Outcome::Unusable(Blob::Held(held), ..) => held,
                Outcome::Got(Blob::Fetched(..)) | Outcome::Unusable(Blob::Fetched(..), ..) => {
                    &stored[f.listed.key.as_str()]
                }
                _ => continue,
            };
            self.0.db.store_blob(tx, &f.listed.key, blake3).await?;
        }
        Ok(())
    }

    fn weight(&self, blob: &Blob) -> usize {
        match blob {
            Blob::Fetched(bytes, _) => bytes.len(),
            Blob::Held(_) | Blob::Missing => 0,
        }
    }
}

impl Attachments<'_> {
    /// One edge's bytes, by the two-hop dance: the file's metadata through
    /// the API, then the signed URL it names, which Azure serves to any
    /// client (the chatgpt cookie is refused there). Bytes the CAS already
    /// holds for the file, under any conversation, cost no request.
    async fn one(&self, key: &str) -> Result<Outcome<Blob>, BatchError> {
        let ctx = self.0;
        let (Some(conv_id), Some((_, file_id))) = (
            ConversationAttachmentRow::owning_id_of(key),
            key.rsplit_once('#'),
        ) else {
            return Ok(Outcome::Failed(format!("{key}: not an attachment key")));
        };
        if let Some(blake3) = ctx
            .db
            .blake3_of_file(file_id)
            .await
            .map_err(BatchError::Abort)?
        {
            ctx.counts.skipped_blobs.fetch_add(1, Ordering::Relaxed);
            return Ok(Outcome::Got(Blob::Held(blake3)));
        }
        let Some(file) = ctx
            .file_ref(conv_id, file_id)
            .await
            .map_err(BatchError::Abort)?
        else {
            // The stored conversation no longer names it: its edge went
            // with the refetch, or will.
            return Ok(Outcome::Gone);
        };
        let meta = match ctx
            .client
            .get(&format!("/backend-api/files/{file_id}/download"))
            .await
        {
            Ok(meta) => meta,
            Err(e) if ctx.stop().requested() => return Err(BatchError::Batch(e.into())),
            Err(e @ ChatGPTError::RateLimited { .. }) => {
                return Err(BatchError::Terminal(e.into()))
            }
            Err(ChatGPTError::Permanent(msg))
                if msg.contains("HTTP 404") || msg.contains("HTTP 410") =>
            {
                return Ok(gone(format!("file metadata: {msg}")));
            }
            Err(e) => return Ok(Outcome::Failed(format!("file metadata: {e}"))),
        };
        let signed = match meta.get("download_url").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Ok(Outcome::Failed(
                    "the file's metadata carries no download URL".to_string(),
                ))
            }
        };
        let req = HttpRequest::get(HttpService::Chatgpt, signed)
            .latchkey(ctx.client.latchkey().clone())
            .timeout(ATTACH_FILE_TIMEOUT);
        match latchkey_curl(&req).await {
            Ok(resp) if (200..300).contains(&resp.status) => {
                ctx.client.count(resp.duration_ms);
                ctx.counts.new_blobs.fetch_add(1, Ordering::Relaxed);
                let content_type = resp.header("content-type").map(String::from);
                Ok(Outcome::Got(Blob::Fetched(
                    resp.body,
                    file.mime.or(content_type),
                )))
            }
            Ok(resp) if matches!(resp.status, 404 | 410) => {
                Ok(gone(format!("signed-URL download HTTP {}", resp.status)))
            }
            Ok(resp) => Ok(Outcome::Failed(format!(
                "signed-URL download HTTP {}",
                resp.status
            ))),
            Err(e) if ctx.stop().requested() => Err(BatchError::Batch(e.into())),
            Err(e @ HttpError::GaveUp { .. }) => Err(BatchError::Terminal(e.into())),
            Err(e) => Ok(Outcome::Failed(format!("signed-URL download: {e}"))),
        }
    }
}

/// A file chatgpt.com no longer has: the edge is held, with a warning,
/// and asked for again only when its conversation changes.
fn gone(why: String) -> Outcome<Blob> {
    Outcome::Unusable(Blob::Missing, Reason::NotFound, why)
}
