//! The render wave for the google_drive source: its planner and the
//! [`RenderProcessor`] it plans.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_google_drive::ingest::{db_path_for, RawDb};
use datalib_etl_google_drive_config::GoogleDriveRenderConfig;
use datalib_etl_render::inputs::{Bucket, Inputs};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

use crate::render::{render_doc, DocInput, RENDER_VERSION};
use crate::{ids, store};

pub fn plan_render(
    ctx: PlanContext,
    config: GoogleDriveRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        GoogleDriveRender,
    ))
}

struct GoogleDriveRender;

#[async_trait]
impl SourceRender for GoogleDriveRender {
    const PROVIDER: &'static str = "google_drive";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    /// A fetch problem names a file by its id; it belongs on that file's
    /// document.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        (table == "files").then(|| ids::file(source_id, id).uuid)
    }

    fn upstream_of_entity(&self, table: &str, id: &str) -> Option<(&'static str, String)> {
        (table == "files").then(|| (ids::KIND_FILE, id.to_string()))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let db_path = db_path_for(raw_path);
        if !db_path.exists() {
            return Ok("no store yet".to_string());
        }
        // On this thread rather than as part of the `Send` future
        // `async_trait` builds: the blob loader's future is not one rustc
        // can prove `Send` for every lifetime.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let Some(db) = RawDb::open_reader(&db_path, ctx.raw_range().pin).await? else {
                    return Ok("nothing committed yet".to_string());
                };
                let result = render_all(&db, ctx).await;
                db.close().await;
                result
            })
        })
    }
}

async fn render_all(db: &RawDb, ctx: &RenderCtx<'_>) -> Result<String> {
    let head = db.pin().map(|p| p.commit().to_string());
    let listing = store::load(db, ctx.raw_range()).await?;
    let mut work: Vec<(String, store::Capture)> = listing
        .captures
        .iter()
        .map(|(file, capture)| (file.clone(), capture.clone()))
        .collect();
    work.sort_by(|a, b| a.0.cmp(&b.0));
    ctx.progress.set_length(Some(listing.docs.len() as u64));

    let mut buckets: Vec<Bucket> = Vec::new();
    let mut rendered = 0;
    let mut failed = 0;
    for (file_id, capture) in work {
        ctx.progress.inc(1);
        let loaded = match store::native(db, &capture).await {
            Ok(native) => store::images(db, &capture)
                .await
                .map(|images| (native, images)),
            Err(e) => Err(e),
        };
        let built = loaded.and_then(|(native, images)| {
            let inputs = Inputs::default();
            let comments: &[(String, serde_json::Value)] = match listing.comments.get(&file_id) {
                Some(c) => c,
                None => &[],
            };
            let md = render_doc(
                ctx.root,
                ctx.name,
                &DocInput {
                    file_id: &file_id,
                    entry: &listing.docs[&file_id],
                    capture: &capture,
                    native: &native,
                    images: &images,
                    comments,
                },
                &inputs,
            )?;
            Ok((md, inputs.declared()))
        });
        match built {
            Ok((md, inputs)) => {
                ctx.emit_doc(md)
                    .with_context(|| format!("store the document for {file_id}"))?;
                buckets.push(Bucket {
                    key: file_id,
                    inputs,
                });
                rendered += 1;
            }
            Err(e) => {
                ctx.fail_bucket(&file_id, &format!("{e:#}"))?;
                failed += 1;
            }
        }
    }
    // A file this run looked at that rendered nothing — its capture or
    // its listing entry gone, or not a Doc — is declared with nothing, so
    // a document it had goes once its rows have.
    let declared: BTreeSet<&str> = buckets.iter().map(|b| b.key.as_str()).collect();
    let empty: Vec<String> = listing
        .render
        .iter()
        .flatten()
        .filter(|k| !declared.contains(k.as_str()))
        .filter(|k| !listing.captures.contains_key(*k) || !listing.docs.contains_key(*k))
        .cloned()
        .collect();
    ctx.declare_empty(empty.iter().map(String::as_str))?;
    ctx.finish(&buckets, head.as_deref())?;
    Ok(format!("rendered {rendered} Doc(s), {failed} failed"))
}
