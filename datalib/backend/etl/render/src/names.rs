//! What a document's rows and source contacts call people and things,
//! settled once for every source as it hands a document over
//! ([`RenderCtx::emit_doc`](crate::processor::RenderCtx::emit_doc)): a
//! name, a subject or a label loses its bidi controls
//! ([`without_bidi_controls`]), and a source contact loses a name that
//! is someone else's handle ([`NormalizedContact::is_anothers_handle`]).
//! The page is the renderer's, and its names go through the shared
//! writers (`Title`, `message::chip_link`, `html::escape_name_inline`).

use datalib_contact_schema::NormalizedContact;

use crate::grid_index::RenderedMarkdown;
use crate::html::{is_bidi_control, without_bidi_controls};

pub(crate) fn settle(md: &mut RenderedMarkdown) {
    for row in &mut md.rows {
        let fields = [
            &mut row.author,
            &mut row.contact,
            &mut row.account,
            &mut row.project,
            &mut row.org_name,
            &mut row.channel,
            &mut row.conversation_name,
        ];
        for s in fields.into_iter().flatten() {
            strip(s);
        }
    }
    for contact in &mut md.contacts {
        settle_contact(contact);
    }
}

fn settle_contact(contact: &mut NormalizedContact) {
    let mut names: Vec<String> = Vec::with_capacity(contact.names.len());
    for mut name in std::mem::take(&mut contact.names) {
        strip(&mut name);
        if !contact.is_anothers_handle(&name) && !names.contains(&name) {
            names.push(name);
        }
    }
    contact.names = names;
    for s in [&mut contact.org, &mut contact.title].into_iter().flatten() {
        strip(s);
    }
    for s in contact.groups.iter_mut().chain(&mut contact.members) {
        strip(s);
    }
}

fn strip(s: &mut String) {
    if s.contains(is_bidi_control) {
        *s = without_bidi_controls(s);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use async_trait::async_trait;
    use datalib_contact_schema::{ContactHandle, ContactKind};
    use datalib_etl::processor::PlanContext;
    use datalib_etl::progress::Progress;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::problems::ProblemRow;
    use datalib_schema::providers::Provider;

    use crate::processor::{plan_source_render, BucketEnd, ReadScope, RenderCtx, SourceRender};

    use super::*;

    /// A source that emits one document, as a provider's render does.
    struct Emits(RenderedMarkdown);

    #[async_trait]
    impl SourceRender for Emits {
        const PROVIDER: &'static str = "test";

        fn render_version(&self) -> u32 {
            1
        }

        async fn run(&self, _raw: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
            ctx.emit_doc(self.0.clone())?;
            Ok("rendered".into())
        }
    }

    /// What the driver's sink receives when `md` is rendered through
    /// the processor production plans for a source.
    async fn emitted(md: RenderedMarkdown) -> RenderedMarkdown {
        let root = tempfile::tempdir().unwrap();
        let progress = Progress::noop();
        let mut got = Vec::new();
        let mut on_doc = |md: RenderedMarkdown| {
            got.push(md);
            Ok(())
        };
        let mut on_declare = |_: &str, _: BucketEnd<'_>| Ok(());
        let mut on_problems = |_: &ReadScope, _: &[ProblemRow]| Ok(());
        let ctx = RenderCtx::new(
            "src",
            root.path(),
            "2369-01-01T00:00:00+00:00",
            &progress,
            None,
            None,
            None,
            &mut on_doc,
            &mut on_declare,
            &mut on_problems,
        );
        let processors =
            plan_source_render(PlanContext { name: "src".into() }, root.path(), Emits(md));
        for p in &processors {
            p.run(&ctx).await.unwrap();
        }
        drop(ctx);
        got.pop().expect("one document")
    }

    fn doc(row: GridRow, contacts: Vec<NormalizedContact>) -> RenderedMarkdown {
        RenderedMarkdown {
            markdown_uuid: "md-1".into(),
            source_id: "src".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: "src/render_markdown/-1/md-1.md".into(),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            section_layout: None,
            content_hash: None,
            edges: Vec::new(),
            contacts,
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn row() -> datalib_schema::grid_rows::GridRowBuilder {
        GridRow::builder()
            .uuid("row-1")
            .provider(Provider::Test)
            .kind("Event")
            .source_label("Test")
            .conversation_uuid("md-1")
            .entire_chat("/chat/md-1")
            .body("Orders \u{202E}exe.live stays as written in a body")
            .is_document(true)
            .item_count(Some(1))
    }

    /// A right-to-left override showed `evil.exe` as `exe.live` in the
    /// grid's Author, Subject and Calendar columns, and in a contact's
    /// name, for any source: none of them went through chat-common.
    #[tokio::test]
    async fn every_source_s_rows_and_contacts_lose_their_bidi_controls() {
        const RLO: &str = "\u{202E}";
        let r = row()
            .author(format!("Pic{RLO}ard"))
            .contact(format!("Wo{RLO}rf"))
            .channel(format!("Bridge {RLO}werC"))
            .conversation_name(format!("Orders {RLO}exe.live"))
            .org_name("\u{2067}Starfleet\u{2069}".to_string())
            .build()
            .unwrap();
        let mut troi = NormalizedContact::new("src", "troi", ContactKind::Person);
        troi.names = vec![format!("Deanna {RLO}Troi"), "Deanna\u{200F}".into()];
        troi.org = Some(format!("{RLO}Betazed"));
        troi.groups = vec![format!("Bridge{RLO}")];
        let got = emitted(doc(r, vec![troi])).await;

        let row = &got.rows[0];
        assert_eq!(row.author.as_deref(), Some("Picard"));
        assert_eq!(row.contact.as_deref(), Some("Worf"));
        assert_eq!(row.channel.as_deref(), Some("Bridge werC"));
        assert_eq!(row.conversation_name.as_deref(), Some("Orders exe.live"));
        assert_eq!(row.org_name.as_deref(), Some("Starfleet"));
        let troi = &got.contacts[0];
        assert_eq!(troi.names, ["Deanna Troi", "Deanna\u{200F}"], "marks stay");
        assert_eq!(troi.org.as_deref(), Some("Betazed"));
        assert_eq!(troi.groups, ["Bridge"]);
    }

    /// A forged `From: "picard@enterprise.org" <q@continuum.test>` named
    /// Q's record after Picard's address, and the search then knew Q by
    /// it. A name that is a handle the record holds stays.
    #[tokio::test]
    async fn a_contact_loses_a_name_that_is_someone_else_s_handle() {
        let mut q = NormalizedContact::new("mail", "email:q@continuum.test", ContactKind::Person);
        q.handles = vec![ContactHandle::email(None, "q@continuum.test")];
        q.names = vec![
            "picard@enterprise.org".into(),
            "picard\u{202E}@enterprise.org".into(),
            "+1 202 555 0101".into(),
            "Q".into(),
            "q@continuum.test".into(),
        ];
        let got = emitted(doc(row().build().unwrap(), vec![q])).await;
        assert_eq!(got.contacts[0].names, ["Q", "q@continuum.test"]);
    }
}
