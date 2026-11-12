//! The render wave for the email source: its planner and the
//! [`RenderProcessor`] it plans.

use crate::render::render::{unmatched_label_rows, OutlinkFormat};
use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_email::ingest;
use datalib_etl_email_config::EmailOutlink;
use datalib_etl_email_config::EmailRenderConfig;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: EmailRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let outlink = config.outlink_format.map(outlink_format);
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        EmailRender {
            outlink,
            only_render_labels: config.only_render_labels.clone(),
        },
    ))
}

/// Email's render processor — reads the raw store and emits one rendered
/// markdown per thread through the fused-Load callback.
pub struct EmailRender {
    outlink: Option<OutlinkFormat>,
    /// Render only threads with at least one email under one of these mailbox
    /// label paths (empty = render everything extracted).
    only_render_labels: Vec<String>,
}

#[async_trait]
impl SourceRender for EmailRender {
    const PROVIDER: &'static str = "email";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    // Both knobs change the rendered output for documents the diff
    // would never surface, so a change to either re-renders everything.
    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(crate::render::render::render_params(
            self.outlink,
            &self.only_render_labels,
        ))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::open;
        use crate::render::render::{render_store, PageEnd};

        let db = ingest::db_path_for(raw_path);
        if !db.exists() {
            tracing::info!(
                source = %ctx.name,
                db = %db.display(),
                "email render: no raw db — skipping",
            );
            return Ok("skipped (no raw db)".into());
        }

        let Some(store) = open(&db, ctx.name, ctx.raw_range(), &self.only_render_labels)? else {
            return Ok("rendered".into());
        };
        // `threads` is read by no render, so a stale parse problem on
        // one is cleared with the rest.
        ctx.report_unparsed(
            &ReadScope::Whole(vec!["accounts", "mailboxes", "threads"]),
            &store.unparsed,
            Some(self.render_version()),
        )?;
        ctx.report_entity_problems(
            &ReadScope::Whole(vec!["config"]),
            &unmatched_label_rows(
                &store.shared,
                ctx.name,
                &self.only_render_labels,
                self.render_version(),
            ),
        )?;
        ctx.declare_empty(store.scan.gone.iter().map(String::as_str))?;
        // Each page is declared before the next is read. A stale thread
        // no email still belongs to builds no chat: declared with
        // nothing, its documents go once the diff says its emails left.
        // One the label filter keeps out goes on the filter's word. The
        // rendered ones follow and replace that.
        let mut on_page = |page: PageEnd| -> Result<()> {
            ctx.declare_empty(page.looked_at.iter().map(String::as_str))?;
            for thread in &page.rendered.excluded {
                ctx.exclude_bucket(thread)?;
            }
            ctx.declare_buckets(&page.rendered.buckets)
        };
        let rendered = render_store(
            &store,
            ctx.root,
            ctx.name,
            self.outlink,
            ctx.page_size,
            ctx.progress,
            &mut |md| ctx.emit_doc(md),
            &mut on_page,
        );
        let head = store.scan.new_head.clone();
        store.close();
        rendered?;
        if let Some(head) = head {
            ctx.consumed(&head);
        }
        Ok("rendered".into())
    }
}

fn outlink_format(f: EmailOutlink) -> OutlinkFormat {
    match f {
        EmailOutlink::Gmail => OutlinkFormat::Gmail,
        EmailOutlink::Fastmail => OutlinkFormat::Fastmail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::doltlite_raw::commit_run;
    use datalib_etl::progress::Progress;
    use datalib_etl_email::ingest::db::RawDb;
    use datalib_etl_render::processor::BucketEnd;
    use datalib_schema::problems::ProblemRow;

    /// What one render reports under each scope, against a store that
    /// holds no mailboxes.
    async fn reports(raw: &Path, only_render_labels: &[&str]) -> Vec<(ReadScope, Vec<ProblemRow>)> {
        let render = EmailRender {
            outlink: None,
            only_render_labels: only_render_labels.iter().map(|l| l.to_string()).collect(),
        };
        let out = tempfile::tempdir().unwrap();
        let progress = Progress::noop();
        let mut reported = Vec::new();
        let mut on_doc = |_| Ok(());
        let mut on_declare = |_: &str, _: BucketEnd<'_>| Ok(());
        let mut on_problems = |scope: &ReadScope, rows: &[ProblemRow]| {
            reported.push((scope.clone(), rows.to_vec()));
            Ok(())
        };
        let ctx = RenderCtx::new(
            "mail",
            out.path(),
            "2364-01-01T00:00:00+00:00",
            &progress,
            None,
            None,
            None,
            &mut on_doc,
            &mut on_declare,
            &mut on_problems,
        );
        render.run(raw, &ctx).await.unwrap();
        drop(ctx);
        reported
    }

    /// The label check reaches the problems table on a real run, and a run
    /// whose config is corrected reports the `config:` scope with nothing
    /// in it, which is what clears the row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_reports_an_unmatched_label_and_a_corrected_run_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
        commit_run(db.pool(), "sync").await.unwrap();
        db.close().await;

        let config = ReadScope::Whole(vec!["config"]);
        let typo = reports(dir.path(), &["Holodek"]).await;
        let rows = &typo.iter().find(|(s, _)| *s == config).unwrap().1;
        assert_eq!(rows.len(), 1, "{typo:?}");
        assert_eq!(rows[0].scope_key, "config:only_render_labels:Holodek");

        let corrected = reports(dir.path(), &[]).await;
        let rows = &corrected.iter().find(|(s, _)| *s == config).unwrap().1;
        assert_eq!(rows, &Vec::<ProblemRow>::new());
    }

    const FORGED: &str = "From 1@xxx Mon Jan 05 09:00:00 +0000 2026\n\
        X-GM-THRID: 1\n\
        X-Gmail-Labels: Spam\n\
        Message-Id: <forged-1@continuum.test>\n\
        From: \"picard@enterprise.org\" <q@continuum.test>\n\
        To: Will Riker <riker@enterprise.org>\n\
        Subject: Orders\n\
        Date: Mon, 5 Jan 2026 09:00:00 +0000\n\
        Content-Type: text/plain; charset=utf-8\n\
        \n\
        Surrender the ship.\n\
        \n\
        From 2@xxx Mon Jan 05 10:00:00 +0000 2026\n\
        X-GM-THRID: 1\n\
        X-Gmail-Labels: Spam\n\
        Message-Id: <forged-2@continuum.test>\n\
        In-Reply-To: <forged-1@continuum.test>\n\
        From: Q <q@continuum.test>\n\
        To: Will Riker <riker@enterprise.org>\n\
        Subject: Re: Orders\n\
        Date: Mon, 5 Jan 2026 10:00:00 +0000\n\
        Content-Type: text/plain; charset=utf-8\n\
        \n\
        Well?\n";

    /// A forged `From: "picard@enterprise.org" <q@continuum.test>` gave
    /// Q's record of the thread Picard's address as a name, and the
    /// search then found Q under it. The name Q gave stays.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sender_shown_as_another_address_is_not_named_by_it() {
        use datalib_etl_email::ingest::mbox;
        let dir = tempfile::tempdir().unwrap();
        let mbox_path = dir.path().join("in.mbox");
        std::fs::write(&mbox_path, FORGED).unwrap();
        let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
        mbox::fetch(mbox::FetchOptions {
            input_path: mbox_path,
            ..mbox::FetchOptions::new(db.clone())
        })
        .await
        .unwrap();
        commit_run(db.pool(), "sync").await.unwrap();
        db.close().await;

        let render = EmailRender {
            outlink: None,
            only_render_labels: Vec::new(),
        };
        let out = tempfile::tempdir().unwrap();
        let progress = Progress::noop();
        let mut docs = Vec::new();
        let mut on_doc = |md| {
            docs.push(md);
            Ok(())
        };
        let mut on_declare = |_: &str, _: BucketEnd<'_>| Ok(());
        let mut on_problems = |_: &ReadScope, _: &[ProblemRow]| Ok(());
        let ctx = RenderCtx::new(
            "mail",
            out.path(),
            "2364-01-01T00:00:00+00:00",
            &progress,
            None,
            None,
            None,
            &mut on_doc,
            &mut on_declare,
            &mut on_problems,
        );
        render.run(dir.path(), &ctx).await.unwrap();
        drop(ctx);

        let q = datalib_handle::Handle::email("q@continuum.test").unwrap();
        let names: Vec<&str> = docs
            .iter()
            .flat_map(|d| &d.contacts)
            .filter(|c| c.holds(&q))
            .flat_map(|c| c.names.iter().map(String::as_str))
            .collect();
        assert_eq!(names, ["Q"]);
    }
}
