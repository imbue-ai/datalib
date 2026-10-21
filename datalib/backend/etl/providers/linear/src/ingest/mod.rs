//! Linear downloader: mirror a workspace's objects, as Linear's GraphQL
//! API answers them, into one raw table per kind.
//!
//! A run is two parts. The workspace's small lists — teams, people,
//! states, labels, cycles, projects, milestones, initiatives — are read
//! whole every run: a few requests, and the only way to see the
//! progress series Linear recomputes without moving a record's
//! `updatedAt`. Then the big ones — issues with their history,
//! comments, documents, project updates, attachments — are each walked
//! newest first over what no earlier run has covered (`walk`), so a run
//! after the first reads only what changed. INGEST.md has the whole
//! account.

pub mod client;
pub mod db;
pub mod fields;
pub mod schema_raw;
pub mod selection;
pub mod walk;

use std::collections::HashSet;

use anyhow::Result;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_linear_config::LinearSync;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};

pub use client::{Api, LinearClient, LinearError, Page, PageQuery};
pub use db::{db_path_for, RawDb};

use db::{id_of, updated_at, Stamp};
use schema_raw::Kind;
use selection::Selection;
use walk::Position;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub sync: LinearSync,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
    pub sealer: Option<Sealer>,
}

impl FetchOptions {
    pub fn new(db: RawDb) -> Self {
        FetchOptions {
            db,
            latchkey: LatchkeySettings::default(),
            sync: LinearSync::default(),
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    /// Records of the whole-read lists.
    pub workspace: usize,
    /// Records the walks listed.
    pub listed: usize,
    /// Of those, the ones written: new, or changed since they were held.
    pub written: usize,
    /// History entries read, with their issues or after them.
    pub history: usize,
    pub requests: u64,
}

/// The run ends here: a refused credential, a spent budget, or a retry
/// guard that gave up would fail every request left.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RunEnded(pub LinearError);

fn run_over(e: LinearError) -> anyhow::Error {
    RunEnded(e).into()
}

/// A connection read whole every run.
struct Listing {
    kind: Kind,
    root: &'static str,
    fields: &'static str,
    extra_args: &'static str,
    page_size: u32,
}

const PROJECTS: Listing = Listing {
    kind: schema_raw::PROJECTS,
    root: "projects",
    fields: fields::PROJECT,
    extra_args: "",
    page_size: 50,
};

const TEAMS: Listing = Listing {
    kind: schema_raw::TEAMS,
    root: "teams",
    fields: fields::TEAM,
    extra_args: "",
    page_size: 100,
};

/// The rest of the whole-read lists, after teams and projects.
const LISTINGS: &[Listing] = &[
    Listing {
        kind: schema_raw::USERS,
        root: "users",
        fields: fields::USER,
        extra_args: "includeDisabled: true",
        page_size: 100,
    },
    Listing {
        kind: schema_raw::WORKFLOW_STATES,
        root: "workflowStates",
        fields: fields::WORKFLOW_STATE,
        extra_args: "",
        page_size: 100,
    },
    Listing {
        kind: schema_raw::ISSUE_LABELS,
        root: "issueLabels",
        fields: fields::ISSUE_LABEL,
        extra_args: "",
        page_size: 100,
    },
    Listing {
        kind: schema_raw::PROJECT_STATUSES,
        root: "projectStatuses",
        fields: fields::PROJECT_STATUS,
        extra_args: "",
        page_size: 100,
    },
    Listing {
        kind: schema_raw::INITIATIVES,
        root: "initiatives",
        fields: fields::INITIATIVE,
        extra_args: "",
        page_size: 50,
    },
    Listing {
        kind: schema_raw::CYCLES,
        root: "cycles",
        fields: fields::CYCLE,
        extra_args: "",
        page_size: 100,
    },
    Listing {
        kind: schema_raw::PROJECT_MILESTONES,
        root: "projectMilestones",
        fields: fields::PROJECT_MILESTONE,
        extra_args: "",
        page_size: 100,
    },
];

/// What a walked connection's page needs before it is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Plain,
    /// The history that came with each issue goes to its own table.
    Issues,
    /// Kept only when its issue is in the selection, and the issue's
    /// team and project dropped once that is decided.
    Attachments,
}

/// A connection walked newest first.
struct Walked {
    kind: Kind,
    root: &'static str,
    fields: String,
    filter_type: &'static str,
    filter: Option<Value>,
    page_size: u32,
    shape: Shape,
}

fn walks(sel: &Selection) -> Vec<Walked> {
    vec![
        Walked {
            kind: schema_raw::ISSUES,
            root: "issues",
            fields: fields::issue(),
            filter_type: "IssueFilter",
            filter: sel.issues(),
            page_size: 50,
            shape: Shape::Issues,
        },
        Walked {
            kind: schema_raw::COMMENTS,
            root: "comments",
            fields: fields::comment(),
            filter_type: "CommentFilter",
            filter: sel.comments(),
            page_size: 100,
            shape: Shape::Plain,
        },
        Walked {
            kind: schema_raw::DOCUMENTS,
            root: "documents",
            fields: fields::DOCUMENT.to_string(),
            filter_type: "DocumentFilter",
            filter: sel.documents(),
            page_size: 50,
            shape: Shape::Plain,
        },
        Walked {
            kind: schema_raw::PROJECT_UPDATES,
            root: "projectUpdates",
            fields: fields::PROJECT_UPDATE.to_string(),
            filter_type: "ProjectUpdateFilter",
            filter: sel.project_updates(),
            page_size: 100,
            shape: Shape::Plain,
        },
        Walked {
            kind: schema_raw::ATTACHMENTS,
            root: "attachments",
            fields: fields::ATTACHMENT.to_string(),
            filter_type: "AttachmentFilter",
            filter: None,
            page_size: 100,
            shape: Shape::Attachments,
        },
    ]
}

struct Ctx<'a> {
    api: &'a dyn Api,
    db: &'a RawDb,
    opts: &'a FetchOptions,
    found: &'a RunProblems,
}

impl Ctx<'_> {
    fn stopped(&self) -> bool {
        self.opts.control.stop.requested()
    }

    async fn wrote(&self, rows: usize) {
        self.opts.progress.inc(rows as u64);
        if let Some(sealer) = &self.opts.sealer {
            sealer.wrote(rows as u64).await;
        }
    }

    /// Every page of `l`. A page that fails part-way leaves what came
    /// before it and a `listing:` row; `None` when not one page came.
    async fn list(&self, l: &Listing) -> Result<Option<Vec<Value>>> {
        self.opts.progress.set_message(l.kind.table);
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            if self.stopped() {
                return Ok(Some(out));
            }
            let q = PageQuery {
                root: l.root,
                fields: l.fields,
                filter_type: "",
                filter: None,
                extra_args: l.extra_args,
                first: l.page_size,
                after: after.clone(),
            };
            match self.api.page(&q).await {
                Ok(page) => {
                    out.extend(page.nodes);
                    if !page.has_next || page.end_cursor.is_none() {
                        return Ok(Some(out));
                    }
                    after = page.end_cursor;
                }
                Err(e) if e.ends_the_run() => return Err(run_over(e)),
                Err(e) => {
                    self.found.listing(
                        l.kind.table,
                        format!(
                            "Linear's list of {} stopped after {} records, so the rest were \
                             left as they were: {e}",
                            l.kind.table,
                            out.len()
                        ),
                    );
                    return Ok((!out.is_empty()).then_some(out));
                }
            }
        }
    }

    async fn store_whole(&self, kind: Kind, records: &[Value], s: &mut FetchSummary) -> Result<()> {
        let now = IsoOffsetTimestamp::now_local();
        let mut tx = self.db.pool().begin().await?;
        let n = db::write(&mut tx, kind, records, Stamp::FirstSeen, &now).await?;
        tx.commit().await?;
        s.workspace += n;
        self.wrote(n).await;
        Ok(())
    }

    /// Teams and projects first, since the selection is resolved
    /// against them, then the rest of the small lists.
    async fn workspace(&self, s: &mut FetchSummary) -> Result<Option<Selection>> {
        let Some(teams) = self.list(&TEAMS).await? else {
            return Ok(None);
        };
        let Some(projects) = self.list(&PROJECTS).await? else {
            return Ok(None);
        };
        let (sel, problems) = selection::resolve(&self.opts.sync, &teams, &projects);
        self.found.config(problems);
        self.store_whole(schema_raw::TEAMS, &teams, s).await?;
        let kept: Vec<Value> = projects
            .into_iter()
            .filter(|p| sel.keeps_project(p))
            .collect();
        self.store_whole(schema_raw::PROJECTS, &kept, s).await?;
        for l in LISTINGS {
            let Some(mut records) = self.list(l).await? else {
                continue;
            };
            if l.kind.table == schema_raw::CYCLES.table {
                records.retain(|c| sel.keeps_cycle(c));
            } else if l.kind.table == schema_raw::PROJECT_MILESTONES.table {
                records.retain(|m| sel.keeps_milestone(m));
            }
            self.store_whole(l.kind, &records, s).await?;
        }
        Ok(Some(sel))
    }

    async fn walk(&self, w: &Walked, sel: &Selection, s: &mut FetchSummary) -> Result<()> {
        self.opts.progress.set_message(w.kind.table);
        let scope = format!("{}:{}", w.kind.table, sel.key());
        let held = coverage::held(self.db.pool(), &scope).await?;
        for gap in walk::gaps_newest_first(&held) {
            let mut pos = Position::start(&gap);
            loop {
                if self.stopped() {
                    return Ok(());
                }
                let q = PageQuery {
                    root: w.root,
                    fields: &w.fields,
                    filter_type: w.filter_type,
                    filter: walk::and(w.filter.clone(), walk::bounds(&gap, &pos)),
                    extra_args: "",
                    first: w.page_size,
                    after: pos.after.clone(),
                };
                let page = match self.api.page(&q).await {
                    Ok(page) => page,
                    Err(_) if self.stopped() => return Ok(()),
                    Err(e) if e.ends_the_run() => return Err(run_over(e)),
                    Err(e) => {
                        self.found.listing(
                            w.kind.table,
                            format!(
                                "the walk of {} stopped part-way, so older changes were not \
                                 looked at; the next run carries on from there: {e}",
                                w.kind.table
                            ),
                        );
                        return Ok(());
                    }
                };
                let stamps: Vec<&str> = page.nodes.iter().filter_map(updated_at).collect();
                let step = walk::step(&gap, &pos, &stamps, page.has_next, page.end_cursor.clone());
                self.store_page(w, sel, page.nodes, step.cover, &scope, s)
                    .await?;
                match step.next {
                    Some(next) => pos = next,
                    None => break,
                }
            }
        }
        Ok(())
    }

    /// Stores the records of one page not already held at their
    /// `updatedAt`, with `cover` claimed, in one transaction.
    async fn store_page(
        &self,
        w: &Walked,
        sel: &Selection,
        mut records: Vec<Value>,
        cover: Option<Span>,
        scope: &str,
        s: &mut FetchSummary,
    ) -> Result<()> {
        let mut history: Vec<Value> = Vec::new();
        let mut more_history: Vec<(String, String)> = Vec::new();
        match w.shape {
            Shape::Plain => {}
            Shape::Issues => {
                for issue in &mut records {
                    let Some(h) = issue.as_object_mut().and_then(|o| o.remove("history")) else {
                        continue;
                    };
                    let page = client::read_page(&json!({ "h": h }), &["h"])
                        .map_err(|e| anyhow::anyhow!("an issue's history: {e}"))?;
                    if let (true, Some(cursor), Some(id)) =
                        (page.has_next, page.end_cursor, id_of(issue))
                    {
                        more_history.push((id.to_string(), cursor));
                    }
                    history.extend(page.nodes);
                }
            }
            Shape::Attachments => {
                records.retain(|a| sel.keeps_attachment(a));
                for a in &mut records {
                    if let Some(issue) = a.get_mut("issue").and_then(Value::as_object_mut) {
                        issue.retain(|k, _| k == "id");
                    }
                }
            }
        }
        s.listed += records.len();

        let table = w.kind.table;
        let ids: Vec<&str> = records.iter().filter_map(id_of).collect();
        let held = owed::held_versions(self.db.pool(), table, ids).await?;
        let changed: Vec<Value> = records
            .into_iter()
            .filter(|r| {
                let version = updated_at(r).map(String::from);
                !id_of(r)
                    .and_then(|id| held.get(id))
                    .is_some_and(|h| h.satisfies(&version))
            })
            .collect();

        let changed_ids: HashSet<&str> = changed.iter().filter_map(id_of).collect();
        for (issue, cursor) in more_history
            .iter()
            .filter(|(id, _)| changed_ids.contains(id.as_str()))
        {
            self.rest_of_history(issue, cursor, &mut history).await?;
        }

        let now = IsoOffsetTimestamp::now_local();
        let mut tx = self.db.pool().begin().await?;
        let n = db::write(&mut tx, w.kind, &changed, Stamp::Fetched, &now).await?;
        for r in &changed {
            if let Some(id) = id_of(r) {
                owed::set_held_version(&mut tx, table, id, updated_at(r)).await?;
            }
        }
        let h = db::write(
            &mut tx,
            schema_raw::ISSUE_HISTORY,
            &history,
            Stamp::FirstSeen,
            &now,
        )
        .await?;
        if let Some(span) = cover {
            coverage::cover(&mut tx, scope, span).await?;
        }
        tx.commit().await?;
        s.written += n;
        s.history += h;
        self.wrote(n + h).await;
        Ok(())
    }

    /// The history entries of `issue` past the ones that came with it.
    /// A page that fails is the issue's problem row; the entries before
    /// it are kept.
    async fn rest_of_history(&self, issue: &str, cursor: &str, out: &mut Vec<Value>) -> Result<()> {
        let mut after = Some(cursor.to_string());
        while let Some(c) = after.take() {
            match self.api.issue_history(issue, Some(&c)).await {
                Ok(page) => {
                    out.extend(page.nodes);
                    if page.has_next {
                        after = page.end_cursor;
                    }
                }
                Err(e) if e.ends_the_run() => return Err(run_over(e)),
                Err(e) => {
                    self.found.record_failed(
                        schema_raw::ISSUES.table,
                        issue,
                        format!("the rest of this issue's history would not come: {e}"),
                    );
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let client = LinearClient::new(opts.latchkey.clone());
    fetch_with(&client, opts).await
}

/// [`fetch`] against any [`Api`]: the live client, or a test's.
pub async fn fetch_with(api: &dyn Api, opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        download(api, opts, found)
    })
    .await
}

async fn download(api: &dyn Api, opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();
    let run_config = json!({ "teams": opts.sync.teams, "projects": opts.sync.projects });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let ctx = Ctx {
        api,
        db: &db,
        opts: &opts,
        found: &found,
    };
    let mut s = FetchSummary::default();
    let result = phases(&ctx, &mut s).await;
    s.requests = api.requests();
    run.finish(&result, &s).await;
    result.map(|()| s)
}

async fn phases(ctx: &Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let ended = match run_phases(ctx, s).await {
        Ok(()) => return Ok(()),
        Err(e) => match e.downcast::<RunEnded>() {
            Ok(ended) => ended,
            Err(e) => return Err(e),
        },
    };
    let nothing_yet = s.workspace == 0 && s.listed == 0;
    if nothing_yet && matches!(ended.0, LinearError::Unauthorized(_)) {
        anyhow::bail!(
            "Linear refused the credential: {}. Sign in again with \
             `latchkey auth browser linear`.",
            ended.0
        );
    }
    let problem = match &ended.0 {
        LinearError::Unauthorized(_) => RunProblem::phase(
            "credential",
            format!(
                "Linear refused the credential part-way through, so the run stopped; what it \
                 fetched before is kept: {}",
                ended.0
            ),
        ),
        _ => RunProblem::phase(
            "rate_limit",
            format!(
                "Linear's hourly request budget ran out, so the run stopped; the next run \
                 carries on from where this one got to: {}",
                ended.0
            ),
        ),
    };
    ctx.found.gave_up(problem.detail.clone());
    ctx.found.push(problem);
    Ok(())
}

async fn run_phases(ctx: &Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let Some(sel) = ctx.workspace(s).await? else {
        anyhow::bail!(
            "Linear's list of teams or projects did not come, so the selection could not be \
             resolved and nothing was walked"
        );
    };
    for w in walks(&sel) {
        ctx.walk(&w, &sel, s).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every walked connection names a filter type Linear has, and the
    /// selection reaches all but attachments, which Linear cannot filter
    /// by team.
    #[test]
    fn a_narrowed_selection_filters_every_walk_but_attachments() {
        let sel = Selection::Some {
            teams: ["t1".to_string()].into(),
            named_projects: Default::default(),
            projects: ["p1".to_string()].into(),
        };
        for w in walks(&sel) {
            assert!(w.filter_type.ends_with("Filter"), "{}", w.root);
            assert_eq!(
                w.filter.is_none(),
                w.shape == Shape::Attachments,
                "{}",
                w.root
            );
        }
        assert!(walks(&Selection::All).iter().all(|w| w.filter.is_none()));
    }
}
