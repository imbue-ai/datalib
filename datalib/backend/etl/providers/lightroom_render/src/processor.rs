//! The render wave for the lightroom source: one document per photo or
//! video, with no file on disk. The `photos` applet draws it from the
//! `source_photos` row the document carries
//! (`docs/dev/plans/photos_applet.md`).

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_lightroom_config::LightroomRenderConfig;
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::inputs::Input;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Scope, Stage};
use datalib_time::NamedZone;

use crate::photo::{self, Context as PhotoContext, RENDER_VERSION};
use crate::read::{self, Catalog};

/// Every photo is filed under one bucket, read from whole tables: a sync
/// where the catalog did not change costs one lookup, and one where it
/// did renders every photo again.
const BUCKET: &str = "catalog";

pub fn plan_render(
    ctx: PlanContext,
    config: LightroomRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    if let Some(name) = &config.capture_time_zone {
        NamedZone::get(name)
            .ok_or_else(|| anyhow!("capture_time_zone {name:?} is not an IANA zone name"))?;
    }
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        LightroomRender {
            capture_time_zone: config.capture_time_zone,
            catalog_path: config.catalog_path,
        },
    ))
}

struct LightroomRender {
    capture_time_zone: Option<String>,
    catalog_path: Option<String>,
}

#[async_trait]
impl SourceRender for LightroomRender {
    const PROVIDER: &'static str = "lightroom";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        serde_json::json!({
            "capture_time_zone": self.capture_time_zone,
            "catalog_path": self.catalog_path,
        })
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let range = ctx.raw_range();
        if let (Some(pin), false) = (range.pin, range.is_stale(BUCKET)) {
            ctx.consumed(pin);
            return Ok(format!("up to date at {pin}"));
        }
        let Some((head, catalog)) = load(raw_path, range.pin)? else {
            return Ok("nothing mirrored yet".to_string());
        };
        let zone = self.capture_time_zone.as_deref().and_then(NamedZone::get);
        let catalog_path = self
            .catalog_path
            .as_deref()
            .or(catalog.catalog_path.as_deref());
        let render_root = datalib_etl::layout::render_markdown_root(ctx.root, ctx.name);
        let mut emitted = 0usize;
        for image in catalog.images.values() {
            let md_path = doc_md_path(&render_root, &photo::uuid(ctx.name, &image.id_global));
            let qmd_path = md_path
                .strip_prefix(ctx.root)
                .unwrap_or(&md_path)
                .to_string_lossy()
                .into_owned();
            let built = photo::build(
                image,
                &PhotoContext {
                    source_id: ctx.name,
                    qmd_path: &qmd_path,
                    catalog: catalog_path,
                    zone: zone.as_ref(),
                },
            );
            ctx.emit_doc(RenderedMarkdown {
                section_layout: None,
                markdown_uuid: built.photo.uuid.clone(),
                source_id: ctx.name.to_string(),
                upstream_cursor: None,
                bucket_key: Some(BUCKET.to_string()),
                // Named but never written: the applet draws the photo.
                md_path,
                render_version: RENDER_VERSION,
                rows: built.row.into_iter().collect(),
                sections: vec![built.section],
                edges: Vec::new(),
                contacts: Vec::new(),
                photos: vec![built.photo],
                problems: built.problems,
            })?;
            emitted += 1;
            ctx.progress.inc(1);
        }
        ctx.report_entity_problems(
            &ReadScope::Whole(vec!["Adobe_AdditionalMetadata"]),
            &xmp_problems(ctx.name, &catalog),
        )?;
        let inputs: Vec<Input> = read::TABLES
            .iter()
            .map(|t| Input::whole_table(*t))
            .collect();
        ctx.finish(
            &[datalib_etl_render::inputs::Bucket {
                key: BUCKET.to_string(),
                inputs,
            }],
            Some(&head),
        )?;
        Ok(format!("photos={emitted}"))
    }
}

/// The mirror at the driver's pin, else HEAD; `None` before anything is
/// committed.
fn load(raw_path: &Path, pin: Option<&str>) -> Result<Option<(String, Catalog)>> {
    let db = datalib_etl::raw_layout::entities_db(raw_path);
    if !db.exists() {
        anyhow::bail!(
            "lightroom mirror not found at {} — run the download step first",
            db.display()
        );
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(reader) = datalib_etl::doltlite_raw::open_reader(&db, pin)
                .await
                .with_context(|| format!("open the lightroom mirror {}", db.display()))?
            else {
                return Ok(None);
            };
            let catalog = read::load(reader.pool()).await;
            let head = reader.pin().commit().to_string();
            reader.close().await;
            Ok(Some((head, catalog?)))
        })
    })
}

/// A source mirrored with `skip_xmp` has no titles and, for most photos,
/// no recorded time zone. Said once, on the table, and cleared by the run
/// that finds the packets again.
fn xmp_problems(source_id: &str, catalog: &Catalog) -> Vec<ProblemRow> {
    if catalog.xmp_mirrored || catalog.images.is_empty() {
        return Vec::new();
    }
    vec![ProblemRow::new(
        source_id,
        Stage::Parse,
        Scope::Entity("Adobe_AdditionalMetadata:xmp"),
        None,
        Outcome::Nulled,
        Problem::lossy(
            "skip_xmp",
            Some("xmp".to_string()),
            "the mirror leaves out each photo's XMP (`skip_xmp`), which is the only place \
             Lightroom keeps a title and the time zone a photo was taken in",
        ),
        Some(RENDER_VERSION),
    )]
}
