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
use datalib_etl::blob_cas::{CasEdgeRow as _, CasInsert};
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

/// What one edge's fetch came to.
pub enum Blob {
    /// The CAS holds the bytes already, under another conversation, by
    /// this hash.
    Held(String),
    /// Bytes this run fetched, and their type. The CAS names them.
    Fetched(Vec<u8>, Option<String>),
    /// A file claude.ai no longer has.
    Missing,
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

impl Files<'_> {
    /// One edge's bytes, from where [`file_url`] says they are. Bytes the
    /// CAS already holds for the file, under any conversation, cost no
    /// request.
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
            return Ok(Outcome::Got(Blob::Held(blake3)));
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
        let org = ctx
            .db
            .org_of_conversation(conv_uuid)
            .await
            .map_err(BatchError::Abort)?;
        let Some(url) = file_url(&file, org.as_deref(), file_uuid) else {
            return Ok(gone(
                "its conversation names no org to ask for the file".to_string(),
            ));
        };
        // For a response that names no type; `file_kind` ("image",
        // "blob") is not one.
        let declared = file
            .get("mime_type")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| {
                file.get("file_name")
                    .and_then(Value::as_str)
                    .and_then(|name| mime_guess::from_path(name).first_raw())
                    .map(String::from)
            });
        let req = HttpRequest::get(HttpService::Claude, &url)
            .latchkey(ctx.client.latchkey().clone())
            .timeout(ATTACH_FILE_TIMEOUT);
        match latchkey_curl(&req).await {
            Ok(resp) if (200..300).contains(&resp.status) => {
                ctx.client.count(resp.duration_ms);
                ctx.counts.new_blobs.fetch_add(1, Ordering::Relaxed);
                let header = resp.header("content-type").map(String::from);
                Ok(Outcome::Got(Blob::Fetched(resp.body, header.or(declared))))
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

/// Where a file's own bytes are. A document's `document_asset.url` is
/// the upload exactly; every other file, a picture or one the sandbox
/// made, is served whole at its org's `/contents`. The `preview_url` a
/// picture names is a re-encoded copy, and a sandbox file names no URL.
pub(crate) fn file_url(file: &Value, org: Option<&str>, file_uuid: &str) -> Option<String> {
    let original = file
        .pointer("/document_asset/url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|path| match path.starts_with("http") {
            true => path.to_string(),
            false => format!("{}{path}", super::CLAUDE_ORIGIN),
        });
    original.or_else(|| {
        org.map(|org| {
            format!(
                "{}/api/organizations/{org}/files/{file_uuid}/contents",
                super::CLAUDE_ORIGIN
            )
        })
    })
}

/// Nothing to fetch: the edge is held, with a warning, and asked for
/// again only when its conversation changes.
fn gone(why: String) -> Outcome<Blob> {
    Outcome::Unusable(Blob::Missing, Reason::NotFound, why)
}

/// The file object with `file_uuid` among `files`.
pub(crate) fn find_file<'a>(files: &'a [Value], file_uuid: &str) -> Option<&'a Value> {
    files.iter().find(|f| file_uuid_of(f) == Some(file_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A file the sandbox made names no URL, and a picture's `preview_url`
    /// is a re-encoded copy: both come whole from the org's `/contents`.
    /// They used to be held as not found, and pictures stored as webp.
    #[test]
    fn a_file_comes_from_its_original() {
        let org = Some("org-1");
        let made = json!({"file_kind": "blob", "file_uuid": "f1", "download_source": "files-api"});
        assert_eq!(
            file_url(&made, org, "f1").as_deref(),
            Some("https://claude.ai/api/organizations/org-1/files/f1/contents")
        );
        let picture = json!({"file_kind": "image", "preview_url": "/api/org-1/files/f2/preview"});
        assert_eq!(
            file_url(&picture, org, "f2").as_deref(),
            Some("https://claude.ai/api/organizations/org-1/files/f2/contents")
        );
        let document = json!({"file_kind": "document",
                              "document_asset": {"url": "/api/org-1/files/f3/document_pdf"}});
        assert_eq!(
            file_url(&document, org, "f3").as_deref(),
            Some("https://claude.ai/api/org-1/files/f3/document_pdf")
        );
        assert_eq!(file_url(&made, None, "f1"), None);
    }
}
