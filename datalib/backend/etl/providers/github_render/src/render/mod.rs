//! Render stage: read the raw store [`datalib_etl_github::ingest`]
//! writes into the forge model, and render one document per PR through
//! `datalib_etl_forge_render_common`.

pub mod ids;
pub mod parse;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_forge_render_common::{render_all, ForgeProfile};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::providers::Provider;

pub use datalib_etl_forge_render_common::{Parsed, RenderSummary, Section};
pub use parse::parse_api_dir;

/// v2: ids are minted through `datalib_id` under `Upstream(repo)`,
///     every row carries its backpointer, and an id carries the
///     record's `created_at` in its leading bits (`datalib_id`'s v8
///     layout). Every uuid moved.
/// v6: a comment's time is a `<time>` stamp.
/// v7: an author with an account is a chip, and the row carries their
///     handle.
/// v8: a change request the signed-in user took part in marks their
///     handle as the account's own, so search reads it as `me`.
/// v9: the description and each comment are a message of their own, and
///     their HTML goes in as written.
/// v10: a comment's time keeps the forge's `Z`: its clock is unknown.
/// v11: the description and each comment open with the one message
///      header every renderer writes, the review's state and `reply` as
///      tags in it.
pub const RENDER_VERSION: u32 = 11;

pub const PROFILE: ForgeProfile = ForgeProfile {
    provider: Provider::Github,
    tag: "github",
    source_label: "GitHub",
    doc_kind: "GitHub PR",
    doc_entity_kind: ids::KIND_PR,
    table: "pull_requests",
    container_key: "repo",
    number_key: "pr_number",
    from_ref_key: "head_ref",
    to_ref_key: "base_ref",
    number_sigil: '#',
    reviews: true,
    render_version: RENDER_VERSION,
};

pub fn render_github(
    parsed: &Parsed,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    render_all(&PROFILE, parsed, stanza, progress, on_doc_complete)
}
