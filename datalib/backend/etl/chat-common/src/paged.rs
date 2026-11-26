//! Rendering a chat store a page of conversations at a time, their blobs
//! a batch at a time, so that nothing about a conversation outlives its
//! page (docs/dev/data_architecture_parse_and_render.md § "Memory: a page
//! at a time"). A provider says how a page is read and how a conversation
//! becomes a chat.

use anyhow::Result;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_render::inputs::RenderUnits;
use datalib_etl_render::pages::{batches, RenderUnitPages};
use datalib_etl_render::processor::{RenderCtx, Unparsed};

use crate::render::ChatRenderer;
use crate::types::NormalizedChat;

/// One page of conversations, no attachment bytes loaded.
pub struct Page<C> {
    pub conversations: Vec<C>,
    /// On a narrowed run, the documents this page looked at, by uuid: each
    /// is declared empty first, so one with no message left goes. Empty
    /// on a full walk, whose conversations all come from rows.
    pub looked_at: Vec<String>,
    pub unparsed: Vec<Unparsed>,
}

pub trait PagedChats {
    type Conversation;

    fn next_page(&self, pages: &mut RenderUnitPages) -> Result<Option<Page<Self::Conversation>>>;

    /// The bytes its attachments take, which its batch is cut by.
    fn attachment_bytes(&self, c: &Self::Conversation) -> u64;

    /// Copies of `batch` with their blobs loaded.
    fn load_blobs(&self, batch: &[&Self::Conversation]) -> Result<Vec<Self::Conversation>>;

    fn chat(&self, c: &Self::Conversation) -> NormalizedChat;

    fn blobs<'c>(&self, c: &'c Self::Conversation) -> &'c BlobBundle;
}

/// How one page ended, declared before the next is read.
pub struct PageEnd {
    pub looked_at: Vec<String>,
    pub render_units: RenderUnits,
    pub unparsed: Vec<Unparsed>,
}

impl PageEnd {
    /// Declare it to the driver; the rows that would not read are added
    /// to `unparsed`, which the run reports once at its end.
    pub fn declare(self, ctx: &RenderCtx<'_>, unparsed: &mut Vec<Unparsed>) -> Result<()> {
        ctx.declare_empty(self.looked_at.iter().map(String::as_str))?;
        ctx.declare_render_units(&self.render_units)?;
        unparsed.extend(self.unparsed);
        Ok(())
    }
}

pub fn render_paged<S: PagedChats>(
    store: &S,
    mut pages: RenderUnitPages,
    blob_bytes: u64,
    renderer: &mut ChatRenderer<'_>,
    on_page: &mut dyn FnMut(PageEnd) -> Result<()>,
) -> Result<()> {
    while let Some(page) = store.next_page(&mut pages)? {
        let weights: Vec<u64> = page
            .conversations
            .iter()
            .map(|c| store.attachment_bytes(c))
            .collect();
        for range in batches(&weights, blob_bytes) {
            let batch: Vec<&S::Conversation> = page.conversations[range].iter().collect();
            for c in store.load_blobs(&batch)? {
                renderer.render_chat(&store.chat(&c), store.blobs(&c))?;
            }
        }
        on_page(PageEnd {
            looked_at: page.looked_at,
            render_units: renderer.take_render_units(),
            unparsed: page.unparsed,
        })?;
    }
    Ok(())
}
