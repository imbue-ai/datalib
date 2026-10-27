//! The render wave for the todoist source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::inputs::{Bucket, Inputs};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_todoist::ingest::{db_path_for, RawDb};
use datalib_etl_todoist_config::TodoistRenderConfig;

use crate::ids;
use crate::render::{render_project, RENDER_VERSION};
use crate::store::{self, flag};

pub fn plan_render(
    ctx: PlanContext,
    config: TodoistRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        TodoistRender,
    ))
}

struct TodoistRender;

#[async_trait]
impl SourceRender for TodoistRender {
    const PROVIDER: &'static str = "todoist";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    /// A fetch problem names a project by its id; it belongs on that
    /// project's document.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        (table == "projects").then(|| ids::project(source_id, id).uuid)
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let db_path = db_path_for(raw_path);
        if !db_path.exists() {
            return Ok("no store yet".to_string());
        }
        let Some(db) = RawDb::open_reader(&db_path, ctx.raw_range().pin).await? else {
            return Ok("nothing committed yet".to_string());
        };
        let result = render_all(&db, ctx).await;
        db.close().await;
        result
    }
}

async fn render_all(db: &RawDb, ctx: &RenderCtx<'_>) -> Result<String> {
    let head = db.pin().map(|p| p.commit().to_string());
    let store = store::load(db, ctx.raw_range()).await?;
    let projects = store.project_ids();
    ctx.progress.set_length(Some(projects.len() as u64));

    let mut buckets: Vec<Bucket> = Vec::new();
    let mut excluded = 0;
    for project in projects.iter().filter(|p| store.renders(p)) {
        ctx.progress.inc(1);
        // A project Todoist deleted keeps its row, marked; its document
        // goes.
        if store
            .projects
            .get(project)
            .is_some_and(|p| flag(p, "is_deleted"))
        {
            ctx.exclude_bucket(project)?;
            excluded += 1;
            continue;
        }
        let inputs = Inputs::default();
        let md = render_project(ctx.root, ctx.name, &store, project, &inputs)?;
        ctx.emit_doc(md)
            .with_context(|| format!("store the document for project {project}"))?;
        buckets.push(Bucket {
            key: project.clone(),
            inputs: inputs.declared(),
        });
    }
    // A stale project the store no longer names at all: its rows left,
    // which the driver reads from the diff.
    let gone: Vec<&String> = store
        .render
        .iter()
        .flatten()
        .filter(|p| !projects.contains(p))
        .collect();
    ctx.declare_empty(gone.iter().map(|s| s.as_str()))?;
    let rendered = buckets.len();
    ctx.finish(&buckets, head.as_deref())?;
    Ok(format!(
        "rendered {rendered} project(s), {excluded} deleted"
    ))
}
