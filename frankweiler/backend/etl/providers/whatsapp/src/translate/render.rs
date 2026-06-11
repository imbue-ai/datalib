//! WhatsApp render — thin adapter over
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! All the work lives in chat-common; this module just builds the
//! [`RenderProfile`] (provider strings + UUID stamp) and forwards.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_chat_common::{
    render::{RenderProfile, RenderSummary},
    NormalizedChat,
};

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing WhatsApp doc rebuilt.
///
/// v1 = chat-common's unified block style + reactions inline +
/// per-message `id="m-{uuid}"` anchors.
pub const RENDER_VERSION: u32 = 1;

const SOURCE_LABEL: &str = "WhatsApp";

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "whatsapp",
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "WhatsApp Chat".to_string(),
        message_kind: "WhatsApp Message".to_string(),
        reaction_kind: "WhatsApp Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

pub fn render_all(
    chats: &[NormalizedChat],
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    frankweiler_etl_chat_common::render::render_all(
        &profile(),
        chats,
        out_dir,
        source_name,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )
}
