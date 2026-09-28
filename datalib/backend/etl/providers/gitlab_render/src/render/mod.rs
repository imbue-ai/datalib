//! Render stage: read the raw store [`datalib_etl_gitlab::ingest`]
//! writes into the forge model, and render one document per MR through
//! `datalib_etl_forge_render_common`.

pub mod ids;
pub mod parse;

use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_forge_render_common::{render_all, ForgeProfile};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::providers::Provider;

pub use datalib_etl_forge_render_common::{Parsed, RenderSummary, Section};
pub use parse::parse_api_dir;

/// v2: ids are minted through `datalib_id` under `Upstream(project)`,
///     every row carries its backpointer, and an id carries the
///     record's `created_at` in its leading bits (`datalib_id`'s v8
///     layout). Every uuid moved.
pub const RENDER_VERSION: u32 = 2;

pub const PROFILE: ForgeProfile = ForgeProfile {
    provider: Provider::Gitlab,
    tag: "gitlab",
    source_label: "GitLab",
    doc_kind: "GitLab MR",
    doc_entity_kind: ids::KIND_MR,
    table: "merge_requests",
    container_key: "project",
    number_key: "mr_iid",
    from_ref_key: "source_branch",
    to_ref_key: "target_branch",
    number_sigil: '!',
    dir_prefix: "mr-",
    container_needs_owner: false,
    reviews: false,
    render_version: RENDER_VERSION,
};

pub fn render_gitlab(
    parsed: &Parsed,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    render_all(&PROFILE, parsed, root, stanza, progress, on_doc_complete)
}
