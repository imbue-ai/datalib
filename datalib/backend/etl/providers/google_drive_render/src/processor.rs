//! The render wave for the google_drive source: its planner and the
//! [`RenderProcessor`] it plans.

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
    for (file_id, capture) in work {
        ctx.progress.inc(1);
        let native = store::native(db, &capture).await?;
        let images = store::images(db, &capture).await?;
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
                native: native.as_ref(),
                images: &images,
                comments,
            },
            &inputs,
        )?;
        ctx.emit_doc(md)
            .with_context(|| format!("store the document for {file_id}"))?;
        buckets.push(Bucket {
            key: file_id,
            inputs: inputs.declared(),
        });
    }
    // A file that is not a Doc, or a Doc with no capture, renders nothing
    // and is known to: whatever document it had goes.
    let no_capture = listing
        .docs
        .keys()
        .filter(|id| !listing.captures.contains_key(*id));
    for id in listing.others.iter().chain(no_capture) {
        ctx.exclude_bucket(id)?;
    }
    // A file whose listing entry is gone: its document goes once its rows
    // have, which the driver reads from the diff.
    let gone: Vec<&String> = listing
        .render
        .iter()
        .flatten()
        .filter(|id| !listing.listed.contains(*id))
        .collect();
    ctx.declare_empty(gone.iter().map(|s| s.as_str()))?;
    let rendered = buckets.len();
    ctx.finish(&buckets, head.as_deref())?;
    Ok(format!("rendered {rendered} Doc(s)"))
}
