//! What each loop fetches and how a flush of it is stored, one
//! [`Fetcher`] per kind: a project's knowledge docs listed whole, a
//! conversation's detail, a file's bytes. Each is one request per
//! record; the loop (`datalib_etl_web::owed`) owns the stop, the failure
//! budget, the flush and what each outcome means for the record's
//! sidecar.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::blob_cas::{blake3_hex, CasEdgeRow as _, CasInsert};
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_problems::Reason;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
use tokio::time::sleep;

use super::api::ClaudeError;
use super::db::{file_uuid_of, Conversation};
use super::schema_raw::ConversationAttachmentRow;
use super::{get_conversation_with_403_retry, parse_conversation, Ctx, ATTACH_FILE_TIMEOUT};

/// What a refused request means for the loop: the run ends on a rate
/// limit, a stop leaves the record to the next run. Anything else is
/// the record's own.
fn ended<T>(ctx: &Ctx<'_>, e: ClaudeError) -> Result<T, BatchError> {
    if ctx.stop().requested() {
        return Err(BatchError::Batch(e.into()));
    }
    match e {
        ClaudeError::RateLimited(_) => Err(BatchError::Terminal(e.into())),
        e => Err(BatchError::Abort(e.into())),
    }
}

// ── knowledge docs ─────────────────────────────────────────────────────

/// What the docs loop knows of each listed project.
pub(crate) struct ProjectListed {
    pub org_uuid: String,
    pub label: String,
}

pub(crate) struct Docs<'a> {
    pub(crate) ctx: &'a Ctx<'a>,
    pub(crate) by_project: HashMap<String, ProjectListed>,
}

#[async_trait]
impl Fetcher<Vec<Value>> for Docs<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Vec<Value>>>, BatchError> {
        let ctx = self.ctx;
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some(project) = self.by_project.get(&listed.key) else {
                continue;
            };
            ctx.bar.doing(&project.label);
            let outcome = match ctx
                .client
                .list_project_docs(&project.org_uuid, &listed.key)
                .await
            {
                Ok(docs) => Outcome::Got(docs),
                // The credential may not read this project's docs. A
                // warning on the listing, asked again next run.
                Err(ClaudeError::Forbidden(msg)) if !ctx.stop().requested() => {
                    Outcome::Skipped(Reason::Forbidden, msg)
                }
                Err(ClaudeError::Permanent(msg)) if !ctx.stop().requested() => Outcome::Failed(msg),
                Err(e) => ended(ctx, e)?,
            };
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Vec<Value>>],
    ) -> Result<()> {
        let ctx = self.ctx;
        for f in batch {
            if let Outcome::Got(docs) | Outcome::Unusable(docs, ..) = &f.outcome {
                let n = ctx
                    .db
                    .store_project_docs(tx, &f.listed.key, docs, &ctx.now, &ctx.run_now)
                    .await?;
                ctx.counts.project_docs.fetch_add(n, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

// ── conversations ──────────────────────────────────────────────────────

pub(crate) struct Conversations<'a> {
    pub(crate) ctx: &'a Ctx<'a>,
    /// The org each listed conversation was listed under.
    pub(crate) org_of: HashMap<String, (String, String)>,
}

#[async_trait]
impl Fetcher<Conversation> for Conversations<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Conversation>>, BatchError> {
        let ctx = self.ctx;
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some((org_uuid, org_name)) = self.org_of.get(&listed.key) else {
                continue;
            };
            ctx.bar.doing(&format!("{org_name} {}", listed.key));
            let outcome = match get_conversation_with_403_retry(ctx.client, org_uuid, &listed.key)
                .await
            {
                Ok(got) => {
                    ctx.counts
                        .forbidden_retry_attempts
                        .fetch_add(got.retries as usize, Ordering::Relaxed);
                    if got.retries > 0 {
                        ctx.counts
                            .forbidden_retry_recoveries
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    match parse_conversation(org_uuid, org_name, &listed.key, &got.value) {
                        Ok(c) => Outcome::Got(c),
                        Err(e) => Outcome::Failed(format!("{e:#}")),
                    }
                }
                Err((e, retries)) => {
                    ctx.counts
                        .forbidden_retry_attempts
                        .fetch_add(retries as usize, Ordering::Relaxed);
                    match e {
                        _ if ctx.stop().requested() => return Err(BatchError::Batch(e.into())),
                        ClaudeError::RateLimited(_) => return Err(BatchError::Terminal(e.into())),
                        e => Outcome::Failed(e.to_string()),
                    }
                }
            };
            ctx.bar.did(1);
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
                    self.ctx.db.store_conversation(tx, c).await?;
                    self.ctx.remember_files(c);
                }
                Outcome::Gone => self.ctx.db.forget_conversation(tx, &f.listed.key).await?,
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        Ok(())
    }

    fn weight(&self, c: &Conversation) -> usize {
        c.payload.len()
    }
}

// ── files ──────────────────────────────────────────────────────────────

/// What one edge's fetch came to: the hash its edge gets, with the
/// bytes behind it when this run fetched them. No hash for a file
/// claude.ai no longer has.
pub struct Blob {
    blake3: Option<String>,
    /// `None` when the CAS holds the bytes already, under another
    /// conversation.
    fetched: Option<(Vec<u8>, Option<String>)>,
}

pub(crate) struct Files<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Blob> for Files<'_> {
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
        let inserts: Vec<CasInsert<'_>> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(blob) | Outcome::Unusable(blob, ..) => {
                    match (&blob.blake3, &blob.fetched) {
                        (Some(blake3), Some((bytes, content_type))) => Some(CasInsert {
                            blake3,
                            bytes,
                            content_type: content_type.as_deref(),
                        }),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        self.0.db.cas().put_many(&inserts).await?;
        for f in batch {
            if let Outcome::Got(blob) | Outcome::Unusable(blob, ..) = &f.outcome {
                if let Some(blake3) = &blob.blake3 {
                    self.0.db.store_blob(tx, &f.listed.key, blake3).await?;
                }
            }
        }
        Ok(())
    }

    fn weight(&self, blob: &Blob) -> usize {
        blob.fetched.as_ref().map_or(0, |(bytes, _)| bytes.len())
    }
}

impl Files<'_> {
    /// One edge's bytes, from the `preview_url` the conversation's file
    /// object names. Bytes the CAS already holds for the file, under any
    /// conversation, cost no request.
    async fn one(&self, key: &str) -> Result<Outcome<Blob>, BatchError> {
        let ctx = self.0;
        let (Some(conv_uuid), Some((_, file_uuid))) = (
            ConversationAttachmentRow::owning_id_of(key),
            key.rsplit_once('#'),
        ) else {
            return Ok(Outcome::Failed(format!("{key}: not an attachment key")));
        };
        if let Some(blake3) = ctx
            .db
            .blake3_of_file(file_uuid)
            .await
            .map_err(BatchError::Abort)?
        {
            ctx.counts.skipped_blobs.fetch_add(1, Ordering::Relaxed);
            return Ok(Outcome::Got(Blob {
                blake3: Some(blake3),
                fetched: None,
            }));
        }
        let Some(file) = ctx
            .file_object(conv_uuid, file_uuid)
            .await
            .map_err(BatchError::Abort)?
        else {
            // The stored conversation no longer names it: its edge went
            // with the refetch, or will.
            return Ok(Outcome::Gone);
        };
        let Some(path) = preview_path(&file) else {
            return Ok(gone("the file has no preview URL".to_string()));
        };
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", super::CLAUDE_ORIGIN)
        };
        let declared = file
            .get("file_kind")
            .and_then(Value::as_str)
            .or_else(|| file.get("mime_type").and_then(Value::as_str))
            .map(String::from);
        let req = HttpRequest::get(HttpService::Claude, &url)
            .latchkey(ctx.client.latchkey().clone())
            .timeout(ATTACH_FILE_TIMEOUT);
        match latchkey_curl(&req).await {
            Ok(resp) if (200..300).contains(&resp.status) => {
                ctx.client.count(resp.duration_ms);
                ctx.counts.new_blobs.fetch_add(1, Ordering::Relaxed);
                let header = resp.header("content-type").map(String::from);
                Ok(Outcome::Got(Blob {
                    blake3: Some(blake3_hex(&resp.body)),
                    fetched: Some((resp.body, header.or(declared))),
                }))
            }
            Ok(resp) if matches!(resp.status, 404 | 410) => Ok(gone(format!(
                "HTTP {}, claude.ai no longer has it: GET {url}",
                resp.status
            ))),
            Ok(resp) => Ok(Outcome::Failed(format!("HTTP {}: GET {url}", resp.status))),
            Err(e) if ctx.stop().requested() => Err(BatchError::Batch(e.into())),
            // Its message carries the tape's path on this machine. The
            // reason leads: the sample is cut at 80 characters.
            Err(HttpError::PlaybackMiss(_)) => {
                Ok(Outcome::Failed(format!("no recorded response: GET {url}")))
            }
            Err(e @ HttpError::GaveUp { .. }) => Err(BatchError::Terminal(e.into())),
            Err(e) => Ok(Outcome::Failed(e.to_string())),
        }
    }
}

/// Where a file object says its bytes are: `preview_url`, else the
/// `document_asset.url`.
fn preview_path(file: &Value) -> Option<&str> {
    file.get("preview_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file.get("document_asset")
                .and_then(|d| d.get("url"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
}

/// Nothing to fetch: the edge is held, with a warning, and asked for
/// again only when its conversation changes.
fn gone(why: String) -> Outcome<Blob> {
    Outcome::Unusable(
        Blob {
            blake3: None,
            fetched: None,
        },
        Reason::NotFound,
        why,
    )
}

/// The file object with `file_uuid` among `files`.
pub(crate) fn find_file<'a>(files: &'a [Value], file_uuid: &str) -> Option<&'a Value> {
    files.iter().find(|f| file_uuid_of(f) == Some(file_uuid))
}
