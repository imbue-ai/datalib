//! The render wave for the baby_connect source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_baby_connect::ingest::{db_path_for, RawDb};
use datalib_etl_baby_connect_config::BabyConnectRenderConfig;
use datalib_etl_render::inputs::{Bucket, Inputs};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

use datalib_time::NamedZone;

use crate::render::{render_child, RENDER_VERSION};
use crate::store;

pub fn plan_render(
    ctx: PlanContext,
    config: BabyConnectRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    config.validate()?;
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        BabyConnectRender {
            time_zone: config.time_zone,
        },
    ))
}

struct BabyConnectRender {
    time_zone: Option<String>,
}

#[async_trait]
impl SourceRender for BabyConnectRender {
    const PROVIDER: &'static str = "baby_connect";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    /// A different zone moves every entry's time, so every child renders
    /// again.
    fn render_params(&self) -> serde_json::Value {
        serde_json::json!({ "time_zone": self.time_zone })
    }

    /// A fetch problem names a day, not a record of a document.
    fn item_of_entity(&self, _source_id: &str, _table: &str, _id: &str) -> Option<String> {
        None
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let db_path = db_path_for(raw_path);
        if !db_path.exists() {
            return Ok("no store yet".to_string());
        }
        let Some(db) = RawDb::open_reader(&db_path, ctx.raw_range().pin).await? else {
            return Ok("nothing committed yet".to_string());
        };
        let zone = self.time_zone.as_deref().and_then(NamedZone::get);
        let result = render_all(&db, ctx, zone.as_ref()).await;
        db.close().await;
        result
    }
}

async fn render_all(db: &RawDb, ctx: &RenderCtx<'_>, zone: Option<&NamedZone>) -> Result<String> {
    let head = db.pin().map(|p| p.commit().to_string());
    let store = store::load(db, ctx.raw_range()).await?;
    let all = store.buckets();
    let todo: Vec<&String> = all.iter().filter(|b| store.renders(b)).collect();
    ctx.progress.set_length(Some(todo.len() as u64));

    let mut buckets: Vec<Bucket> = Vec::new();
    for key in todo {
        ctx.progress.inc(1);
        let inputs = Inputs::default();
        let md = render_child(ctx.root, ctx.name, &store, key, zone, &inputs)?;
        ctx.emit_doc(md)
            .with_context(|| format!("store the document for {key}"))?;
        buckets.push(Bucket {
            key: key.clone(),
            inputs: inputs.declared(),
        });
    }
    // A child the store no longer has an entry for: their rows leave.
    let gone: Vec<&String> = store
        .render
        .iter()
        .flatten()
        .filter(|b| !all.contains(b))
        .collect();
    ctx.declare_empty(gone.iter().map(|s| s.as_str()))?;
    let rendered = buckets.len();
    ctx.finish(&buckets, head.as_deref())?;
    Ok(format!("rendered {rendered} child(ren)"))
}
