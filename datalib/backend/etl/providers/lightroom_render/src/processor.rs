//! The render wave for the lightroom source: one document per photo or
//! video, with no file on disk. The `photos` applet draws it from the
//! `source_photos` row the document carries
//! (`lightroom/INGEST.md` § "Rendering").

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_lightroom_config::LightroomRenderConfig;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::Input;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Scope, Stage};
use datalib_time::NamedZone;

use crate::photo::{self, Context as PhotoContext, RENDER_VERSION};
use crate::read::{self, Catalog};
use crate::tree;
use datalib_schema::grid_rows::GridRow;

/// Every photo is filed under one render unit, read from whole tables: a sync
/// where the catalog did not change costs one lookup, and one where it
/// did renders every photo again.
const RENDER_UNIT: &str = "catalog";

pub fn plan_render(
    ctx: PlanContext,
    config: LightroomRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    if let Some(name) = &config.capture_time_zone {
        NamedZone::get(name)
            .ok_or_else(|| anyhow!("capture_time_zone {name:?} is not a time zone name"))?;
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
        if let (Some(pin), false) = (range.pin, range.is_stale(RENDER_UNIT)) {
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
        let mut emitted = 0usize;
        for image in catalog.images.values() {
            let built = photo::build(
                image,
                &PhotoContext {
                    source_id: ctx.name,
                    catalog: catalog_path,
                    zone: zone.as_ref(),
                },
            );
            let tree = tree::of(ctx.name, image, catalog_name(catalog_path));
            let row = built.row.map(|r| GridRow {
                parent_uuid: tree.photo.parent_uuid.clone(),
                read_uuid: tree.photo.read_uuid.clone(),
                place_uuid: tree.photo.place_uuid.clone(),
                ..r
            });
            ctx.emit_doc(RenderedMarkdown {
                searchable: true,
                content_hash: None,
                markdown_uuid: built.photo.uuid.clone(),
                source_id: ctx.name.to_string(),
                upstream_cursor: None,
                render_unit_key: Some(RENDER_UNIT.to_string()),
                render_version: RENDER_VERSION,
                rows: row.into_iter().collect(),
                sections: vec![built.section],
                front: built.front,
                edges: Vec::new(),
                contacts: Vec::new(),
                data_nodes: tree.data_nodes,
                filings: tree.filings,
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
            &[datalib_etl_render::inputs::RenderUnit {
                key: RENDER_UNIT.to_string(),
                inputs,
            }],
            Some(&head),
        )?;
        Ok(format!("photos={emitted}"))
    }
}

/// The catalog file's name without its folder or `.lrcat`.
fn catalog_name(path: Option<&str>) -> Option<&str> {
    let file = path?.rsplit(['/', '\\']).next()?;
    Some(file.strip_suffix(".lrcat").unwrap_or(file)).filter(|n| !n.is_empty())
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
        Problem::lossy_explained(
            "skip_xmp",
            Some("xmp".to_string()),
            "the mirror leaves out each photo's XMP (`skip_xmp`), which is the only place \
             Lightroom keeps a title and the time zone a photo was taken in",
        ),
        Some(RENDER_VERSION),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `skip_xmp` warning names both things the source goes without;
    /// cut at 80 characters it stopped before either.
    #[test]
    fn the_skip_xmp_warning_is_kept_whole() {
        let catalog = Catalog {
            images: [(1, read::ImageRow::default())].into(),
            ..Catalog::default()
        };
        let rows = xmp_problems("lightroom", &catalog);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0]
                .sample
                .ends_with("the time zone a photo was taken in"),
            "{}",
            rows[0].sample
        );
        assert_eq!(rows[0].rule.as_deref(), Some("skip_xmp"));
    }
}
