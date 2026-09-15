//! `GET /api/manage/rows`: the Manage screen's tree, assembled. One row
//! per entry in the config *file* — a group with its steps and applets
//! under it — with the status, timestamps, sizes and actions the screen
//! draws, joined here from the config, the runner's record, the run
//! store, the job queue, the usage sampler and the applet supervisor.
//! What stays in the browser is what needs the wizard's catalog: type
//! labels and icons, whether the form can edit a row, and what Browse
//! opens.

mod group;
mod status;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Query, State};
use axum::Json;
use datalib_dag::written::{WrittenApplet, WrittenEntries, WrittenGroup, WrittenStep};
use datalib_dag::{Diagnostic, EntryKind, Severity};
use serde::{Deserialize, Serialize};

use crate::usage::{OutputStorage, UsageSample};
use crate::{usage, AppState, DagRecord, DagRunInfo, DagStepProgress};
use group::{Child, ChildKind, ChildStamp, ChildStatus};
use status::{EffectiveRun, StatusArgs, StatusFloor, StatusView, StepEdges, StepRecord};

pub use status::dropped_detail;

/// The floor that keeps a row's status from going backwards within one
/// run (`StatusFloor`). One per process, which is one per data root:
/// the lock in `crate::lock` sees to that.
static FLOOR: LazyLock<Mutex<StatusFloor>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    Group,
    Step,
    Applet,
}

/// The built-in functions by what they do. Mirrors
/// `datalib_step::function::Function`; a step outside any group has no
/// function and is a custom executable, `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ingest,
    Render,
    Index,
    Other,
}

impl Phase {
    fn of_function(function: Option<&str>) -> Self {
        match function {
            Some("ingest") => Phase::Ingest,
            Some("render_markdown") => Phase::Render,
            Some("grid_index") | Some("qmd_index") => Phase::Index,
            _ => Phase::Other,
        }
    }
}

/// One segment of a group's in-flight progress bar: a step and the
/// status it is drawn in.
#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub id: String,
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManageRow {
    /// Identity: the tree this entry writes — for a group, the
    /// directory its steps write into — and what every action is
    /// keyed on.
    pub id: String,
    /// The grid's row id. The entry id for a step or an applet; for a
    /// group, `group:<id>`, because the `unified_index` applet shares
    /// its group's id and both are rows.
    pub key: String,
    /// Where the row sits in the tree: `[key]` at the top level, or
    /// `[<group key>, key]` under its group.
    pub path: Vec<String>,
    pub kind: RowKind,
    /// The group this entry is filed under, when the config declares
    /// it. An entry naming a group the config lacks is shown at the top
    /// level, where its dropped status says what is wrong.
    pub group: Option<String>,
    /// The group written on the entry, declared or not. What Edit
    /// looks up.
    pub written_group: Option<String>,
    pub inputs: Vec<String>,
    pub phase: Phase,
    pub function: Option<String>,
    pub r#type: Option<String>,
    /// What the Name column shows. Under a group a step is labelled by
    /// what it does there ("Render markdown"); the browser reads an
    /// ingest step's "Download" / "Import" off `params` against what
    /// its provider declares, and overrides this one.
    pub name: String,
    /// A step's `params`, as JSON. `{}` off a step.
    pub params: serde_json::Value,
    /// The loader's reason this entry is not in the pipeline, or null
    /// if it is. A dropped entry still has a row — it is still in the
    /// file, and the file is what the user edits.
    pub dropped: Option<Diagnostic>,
    pub status: StatusView,
    /// For a group row, the child whose status it shows — the row a
    /// double-click on Status opens the log of.
    pub status_from: Option<String>,
    pub last_synced: Option<String>,
    /// For a group row with a run in flight: its steps in pipeline
    /// order, one segment each. Null when idle or not a group.
    pub segments: Option<Vec<Segment>>,
    /// What a sync of this row starts at: the step itself, or for a
    /// group its steps with no inputs. Empty exactly when `run_blocked`
    /// says why.
    pub seeds: Vec<String>,
    /// Null when the action applies; otherwise the reason it doesn't,
    /// which becomes the disabled button's tooltip.
    pub run_blocked: Option<String>,
    pub reveal_blocked: Option<String>,
    /// The active job that has claimed this step, when one has.
    /// Non-null is exactly the condition that turns Run into Stop.
    pub stop_job_id: Option<String>,
    /// The step to call the job off through — this step, or for a
    /// group the child that holds the claim.
    pub stop_target: Option<String>,
    /// The Stop button's tooltip.
    pub stop_label: Option<String>,
    /// Why the Stop button takes no click: the job has already been
    /// told to stop and its steps are winding down. Null while a click
    /// would do something.
    pub stop_blocked: Option<String>,
    /// What the step has reported in the run in flight. Null when it
    /// isn't running or hasn't reported anything.
    pub progress: Option<DagStepProgress>,
    /// The run the step's `last_run` happened in — where its log is.
    /// Empty when it has never run, or ran before runs had ids.
    pub last_run_id: String,
    /// The run in flight, when this step is in it — where its live log
    /// is.
    pub live_run_id: Option<String>,
    /// Null when nothing is on disk yet — rendered as "—", not "0 B".
    pub bytes: Option<u64>,
    /// Recent measurements of this row's tree, oldest first.
    pub history: Vec<UsageSample>,
    /// Storage rows for this entry's declared outputs — for a group,
    /// its children's, for the tooltip's breakdown.
    pub outputs: Vec<OutputStorage>,
    /// Absolute path to reveal: the first output that exists.
    pub reveal_path: Option<String>,
}

/// The data root as a whole, for the status bar.
#[derive(Debug, Clone, Serialize)]
pub struct RootStorage {
    pub root: OutputStorage,
    pub window_secs: u64,
    pub measured_at_utc: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ManageResponse {
    pub ok: bool,
    /// Why there are no rows: the file is not TOML at all. An entry
    /// with a problem is a row with a `dropped` reason, not an error.
    pub error: Option<String>,
    /// The run in flight, or the one that finished last.
    pub run: Option<DagRunInfo>,
    pub storage: RootStorage,
    pub rows: Vec<ManageRow>,
}

#[derive(Debug, Deserialize)]
pub struct ManageParams {
    /// Walk the disk before answering rather than serving the sampler's
    /// last tick. Lenient like `/api/pipeline/storage`'s.
    #[serde(default)]
    refresh: Option<String>,
}

pub async fn get_manage_rows(
    State(s): State<AppState>,
    Query(p): Query<ManageParams>,
) -> Json<ManageResponse> {
    if crate::flag_is_set(p.refresh.as_deref()) {
        usage::sample_on_demand(&s.usage, &s.app, s.root.clone()).await;
    }
    let config_path = s.config_path();
    let text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let record = crate::dag_record(&s.root).await;
    let storage = s
        .usage
        .snapshot(s.root.as_path(), &usage::declared_trees(&config_path))
        .await;
    let root_storage = RootStorage {
        root: storage.root.clone(),
        window_secs: storage.window_secs,
        measured_at_utc: storage.measured_at_utc.clone(),
    };
    let written = match datalib_dag::written::entries_as_written(&text) {
        Ok(w) => w,
        Err(e) => {
            return Json(ManageResponse {
                ok: false,
                error: Some(e),
                run: record.run,
                storage: root_storage,
                rows: Vec::new(),
            })
        }
    };
    let diagnostics = datalib_dag::config::check_text(&text).diagnostics;
    // The grid is still useful without the queue; the columns it feeds
    // just read as idle.
    let jobs = s.app.list_jobs(false, 200).await.unwrap_or_default();
    let applet_errors = s.applets.frontend_view().applet_errors;

    let rows = {
        let mut floor = FLOOR.lock().unwrap_or_else(|e| e.into_inner());
        Snapshot {
            written: &written,
            diagnostics: &diagnostics,
            record: &record,
            jobs: &jobs,
            outputs: &storage.outputs,
            applet_errors: &applet_errors,
        }
        .rows(&mut floor)
    };
    Json(ManageResponse {
        ok: true,
        error: None,
        run: record.run,
        storage: root_storage,
        rows,
    })
}

/// Everything one answer is assembled from, read once.
struct Snapshot<'a> {
    written: &'a WrittenEntries,
    diagnostics: &'a [Diagnostic],
    record: &'a DagRecord,
    jobs: &'a [app_schema::sync_jobs::SyncJobRow],
    outputs: &'a [OutputStorage],
    applet_errors: &'a std::collections::BTreeMap<String, String>,
}

/// A step or applet as written, in the one shape the group rules read.
#[derive(Clone)]
enum Entry<'a> {
    Step(&'a WrittenStep),
    Applet(&'a WrittenApplet),
}

impl Entry<'_> {
    fn group(&self) -> Option<&str> {
        match self {
            Entry::Step(s) => s.group.as_deref(),
            Entry::Applet(a) => a.group.as_deref(),
        }
    }
    fn entry_kind(&self) -> EntryKind {
        match self {
            Entry::Step(_) => EntryKind::Step,
            Entry::Applet(_) => EntryKind::Applet,
        }
    }
}

impl Child for Entry<'_> {
    fn id(&self) -> &str {
        match self {
            Entry::Step(s) => &s.id,
            Entry::Applet(a) => &a.id,
        }
    }
    fn kind(&self) -> ChildKind {
        match self {
            Entry::Step(_) => ChildKind::Step,
            Entry::Applet(_) => ChildKind::Applet,
        }
    }
    fn inputs(&self) -> &[String] {
        match self {
            Entry::Step(s) => &s.inputs,
            Entry::Applet(_) => &[],
        }
    }
}

/// What to call the shared entries when nobody has named them. A
/// `name =` someone did set still wins, and the id stays visible beside
/// the name in the grid.
fn default_name(id: &str) -> String {
    match id {
        "unified_index/grid_index" => "Unified Index (table)",
        "unified_index/qmd_index" => "Unified Index (QMD)",
        "unified_index" => "Unified Index (Applet)",
        other => other,
    }
    .to_string()
}

/// What a step under a group is called in the Name column. Derived
/// from the function, never written: the group owns the name, and a
/// step's label says what it does with that group's data.
fn child_label(step: &WrittenStep) -> String {
    match step.function.as_deref() {
        Some("ingest") => "Ingest",
        Some("render_markdown") => "Render markdown",
        Some("grid_index") => "Grid index",
        Some("qmd_index") => "QMD index",
        Some(other) => other,
        None => "Step",
    }
    .to_string()
}

/// The sentence the Status cell carries for an entry the loader dropped.
fn not_in_pipeline(d: &Diagnostic) -> String {
    format!("Not in the pipeline: {}", dropped_detail(d))
}

impl Snapshot<'_> {
    fn rows(&self, floor: &mut StatusFloor) -> Vec<ManageRow> {
        let edges: Vec<StepEdges> = self
            .written
            .steps
            .iter()
            .map(|s| StepEdges {
                id: s.id.clone(),
                inputs: s.inputs.clone(),
            })
            .collect();
        let claims = status::claimed_by(&edges, self.jobs);
        let live_job = self.jobs.iter().find(|j| j.state == "running");
        let run = status::effective_run(self.record.run.as_ref(), live_job);
        let stale = run.as_ref().is_some_and(|r| r.synthesized);
        let ctx = RowCtx {
            snap: self,
            edges: &edges,
            claims: &claims,
            run: run.as_ref(),
            stale,
        };

        let entries: Vec<Entry<'_>> = self
            .written
            .steps
            .iter()
            .map(Entry::Step)
            .chain(self.written.applets.iter().map(Entry::Applet))
            .collect();
        let mut entry_rows: Vec<(Entry<'_>, ManageRow)> = entries
            .iter()
            .map(|e| (e.clone(), ctx.entry_row(e, floor)))
            .collect();

        let groups: Vec<ManageRow> = self
            .written
            .groups
            .iter()
            .map(|g| {
                let children: Vec<&(Entry<'_>, ManageRow)> = entry_rows
                    .iter()
                    .filter(|(_, r)| r.group.as_deref() == Some(g.id.as_str()))
                    .collect();
                ctx.group_row(g, &children)
            })
            .collect();
        let mut rows = groups;
        rows.extend(entry_rows.drain(..).map(|(_, r)| r));
        rows
    }
}

struct RowCtx<'a> {
    snap: &'a Snapshot<'a>,
    edges: &'a [StepEdges],
    claims: &'a HashMap<String, &'a app_schema::sync_jobs::SyncJobRow>,
    run: Option<&'a EffectiveRun>,
    stale: bool,
}

impl RowCtx<'_> {
    /// The reason this entry is not in the pipeline, or None if it is.
    /// Keyed on the kind as well as the id: a group and an applet may
    /// share an id, and a problem with one is not a problem with the
    /// other.
    fn dropped(&self, id: &str, kind: EntryKind) -> Option<&Diagnostic> {
        self.snap.diagnostics.iter().find(|d| {
            d.severity != Severity::Warning
                && d.entry
                    .as_ref()
                    .is_some_and(|e| e.kind == kind && e.id.as_deref() == Some(id))
        })
    }

    fn declared_group(&self, written: Option<&str>) -> Option<String> {
        let g = written?;
        self.snap
            .written
            .groups
            .iter()
            .any(|x| x.id == g)
            .then(|| g.to_string())
    }

    /// The runner's record for one step, as it applies to the run in
    /// flight: `current_state` dropped when the record still describes
    /// a previous run.
    fn step_now(&self, id: &str) -> Option<StepRecord> {
        let record = self.snap.record;
        let last_run = record.last_runs.get(id).cloned();
        let current_state = record.states.get(id).cloned();
        let rec = (last_run.is_some() || current_state.is_some()).then_some(StepRecord {
            last_run,
            current_state,
        });
        status::step_for_run(rec.as_ref(), self.stale)
    }

    fn finished_this_run(&self, id: &str) -> bool {
        self.step_now(id)
            .and_then(|s| s.current_state)
            .is_some_and(|st| st != "running")
    }

    fn step_status(
        &self,
        id: &str,
        dropped: Option<&Diagnostic>,
        floor: &mut StatusFloor,
    ) -> StatusView {
        let claim = self.claims.get(id).copied();
        let step = self.step_now(id);
        let waiting = status::waiting_on(self.edges, id, |i| self.finished_this_run(i));
        let view = status::step_status(StatusArgs {
            id,
            step: step.as_ref(),
            run: self.run,
            claim,
            waiting_on: &waiting,
            dropped,
        });
        let key = claim
            .map(|j| j.id.as_str())
            .or(self.run.map(|r| r.run_id.as_str()))
            .unwrap_or("");
        floor.hold(id, key, view)
    }

    fn stop_label(&self, id: &str) -> Option<String> {
        let job = self.claims.get(id)?;
        let of = match job.source_ids.as_deref().filter(|s| !s.is_empty()) {
            Some(ids) => format!("the sync of {ids}"),
            None => "the sync in progress".to_string(),
        };
        Some(if status::job_stopping(job) {
            format!("Stopping {of}")
        } else {
            format!("Stop {of}")
        })
    }

    // Once asked to stop there is nothing more to ask: the steps in
    // flight are checkpointing, and the face says so until they exit.
    fn stop_blocked(&self, id: &str) -> Option<String> {
        let job = self.claims.get(id)?;
        if !status::job_stopping(job) {
            return None;
        }
        Some(format!(
            "{} \u{2014} its steps are checkpointing and exiting.",
            self.stop_label(id)?
        ))
    }

    fn entry_row(&self, e: &Entry<'_>, floor: &mut StatusFloor) -> ManageRow {
        let id = e.id().to_string();
        let dropped = self.dropped(&id, e.entry_kind());
        let dropped_why = dropped.map(not_in_pipeline);
        let group = self.declared_group(e.group());
        let outputs = self.snap.outputs;
        // A step writes exactly one tree, and it is the step's id.
        let trees: Vec<OutputStorage> = match e {
            Entry::Step(_) => outputs.iter().filter(|o| o.path == id).cloned().collect(),
            Entry::Applet(_) => Vec::new(),
        };
        let on_disk: Vec<&OutputStorage> = trees.iter().filter(|o| o.present).collect();

        let (status, run_blocked, seeds, progress, last_run_id, live_run_id) = match e {
            Entry::Step(s) => {
                let status = self.step_status(&id, dropped, floor);
                // A sync starts at a *source* step — one with no declared
                // inputs — and everything downstream follows.
                // `datalib-dag` rejects a `--sync` naming anything else.
                let fed_by = status::sources_feeding(self.edges, &id);
                let run_blocked = dropped_why.clone().or_else(|| {
                    if s.inputs.is_empty() {
                        None
                    } else if fed_by.len() == 1 {
                        Some(format!(
                            "A sync starts at a source step. Run {} \u{2014} this runs with it.",
                            fed_by[0]
                        ))
                    } else {
                        let list = if fed_by.is_empty() {
                            "none it can reach".to_string()
                        } else {
                            fed_by.join(", ")
                        };
                        Some(format!(
                            "A sync starts at a source step. This one runs whenever any of its \
                             sources does: {list}."
                        ))
                    }
                });
                let seeds = if s.inputs.is_empty() {
                    vec![id.clone()]
                } else {
                    vec![]
                };
                let now = self.step_now(&id);
                let progress = self
                    .snap
                    .record
                    .progress
                    .get(&id)
                    .filter(|_| !self.stale)
                    .cloned();
                let last_run_id = self
                    .snap
                    .record
                    .last_runs
                    .get(&id)
                    .map(|r| r.run_id.clone())
                    .unwrap_or_default();
                let live_run_id = self
                    .run
                    .filter(|r| r.in_flight())
                    .filter(|_| now.as_ref().is_some_and(|n| n.current_state.is_some()))
                    .map(|r| r.run_id.clone());
                (
                    status,
                    run_blocked,
                    seeds,
                    progress,
                    last_run_id,
                    live_run_id,
                )
            }
            Entry::Applet(_) => {
                // An applet's health is its own thing: it isn't
                // scheduled, so the runner's record says nothing about
                // it. The supervisor does. There is no history to show,
                // which is why the timestamp stays null.
                let status = if let Some(d) = dropped {
                    status::view("config_rejected", None, Some(not_in_pipeline(d)))
                } else if let Some(err) = self.snap.applet_errors.get(&id) {
                    StatusView {
                        key: "failed".into(),
                        label: "Failed to start".into(),
                        at: None,
                        detail: Some(err.clone()),
                    }
                } else {
                    StatusView {
                        key: "succeeded".into(),
                        label: "Up".into(),
                        at: None,
                        detail: Some("The gateway has this applet up.".into()),
                    }
                };
                (
                    status,
                    Some(
                        "Applets aren't scheduled \u{2014} the server starts one when something asks for it."
                            .to_string(),
                    ),
                    vec![],
                    None,
                    String::new(),
                    None,
                )
            }
        };

        let reveal_blocked = match e {
            Entry::Applet(_) => {
                Some("An applet owns no files \u{2014} it serves endpoints.".to_string())
            }
            Entry::Step(_) if on_disk.is_empty() => {
                Some("Nothing on disk yet \u{2014} this hasn't produced anything.".to_string())
            }
            Entry::Step(_) => None,
        };

        let (name, params, function, phase, r#type) = match e {
            Entry::Step(s) => {
                // Under a group the name is the group's; the step's
                // label says what it does there. At the top level the
                // step is its own thing and keeps the name the config
                // gave it.
                let name = if group.is_some() {
                    child_label(s)
                } else {
                    s.name.clone().unwrap_or_else(|| default_name(&id))
                };
                let r#type = s
                    .group
                    .as_deref()
                    .and_then(|g| self.snap.written.groups.iter().find(|x| x.id == g))
                    .and_then(|g| g.r#type.clone());
                (
                    name,
                    s.params.clone(),
                    s.function.clone(),
                    Phase::of_function(s.function.as_deref()),
                    r#type,
                )
            }
            Entry::Applet(a) => (
                default_name(&id),
                serde_json::Value::Object(Default::default()),
                None,
                Phase::Other,
                a.r#type.clone(),
            ),
        };

        let stop_job_id = self.claims.get(&id).map(|j| j.id.clone());
        ManageRow {
            key: id.clone(),
            path: match &group {
                Some(g) => vec![group::group_row_key(g), id.clone()],
                None => vec![id.clone()],
            },
            kind: match e {
                Entry::Step(_) => RowKind::Step,
                Entry::Applet(_) => RowKind::Applet,
            },
            written_group: e.group().map(str::to_string),
            group,
            inputs: e.inputs().to_vec(),
            phase,
            function,
            r#type,
            name,
            params,
            dropped: dropped.cloned(),
            last_synced: status.at.clone(),
            status,
            status_from: None,
            segments: None,
            seeds,
            run_blocked,
            reveal_blocked,
            stop_target: stop_job_id.as_ref().map(|_| id.clone()),
            stop_label: self.stop_label(&id),
            stop_blocked: self.stop_blocked(&id),
            stop_job_id,
            progress,
            last_run_id,
            live_run_id,
            bytes: (!on_disk.is_empty()).then(|| trees.iter().map(|o| o.bytes).sum()),
            history: trees.first().map(|o| o.history.clone()).unwrap_or_default(),
            reveal_path: on_disk.first().map(|o| o.abs.clone()),
            outputs: trees,
            id,
        }
    }

    /// The row for one `[[groups]]` entry, read off its children's rows.
    fn group_row(&self, g: &WrittenGroup, children: &[&(Entry<'_>, ManageRow)]) -> ManageRow {
        let entries: Vec<Entry<'_>> = children.iter().map(|(e, _)| e.clone()).collect();
        let ordered = group::pipeline_order(&entries);
        let row_of = |id: &str| -> &ManageRow {
            &children
                .iter()
                .find(|(e, _)| e.id() == id)
                .expect("ordered children come from this group's rows")
                .1
        };
        let steps: Vec<&Entry<'_>> = ordered
            .iter()
            .filter(|c| c.kind() == ChildKind::Step)
            .collect();
        let dropped = self.dropped(&g.id, EntryKind::Group);
        let dropped_why = dropped.map(not_in_pipeline);

        let agg = group::group_status(
            &ordered
                .iter()
                .map(|c| ChildStatus {
                    id: c.id().to_string(),
                    kind: c.kind(),
                    status: row_of(c.id()).status.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let (status, status_from) = if let Some(d) = dropped {
            (
                status::step_status(StatusArgs {
                    id: &g.id,
                    step: None,
                    run: None,
                    claim: None,
                    waiting_on: &[],
                    dropped: Some(d),
                }),
                None,
            )
        } else if let Some((status, from)) = agg {
            (status, Some(from))
        } else {
            (
                status::view(
                    "never_run",
                    None,
                    Some("Nothing is filed under this group yet.".into()),
                ),
                None,
            )
        };

        // The folder the group's steps write into, measured as a tree
        // of its own by the usage walker — not the sum of two series
        // sampled at different instants.
        let tree = self.snap.outputs.iter().find(|o| o.path == g.id);
        let on_disk = tree.is_some_and(|t| t.present);

        let seeds = group::group_seeds(&ordered, |c| row_of(c.id()).dropped.is_some());
        let run_blocked = dropped_why.or_else(|| {
            if !seeds.is_empty() {
                None
            } else if !steps.is_empty() {
                Some(
                    "A sync starts at a source step, and none of this group's steps is one \u{2014} \
                     they run whenever the sources feeding them do."
                        .to_string(),
                )
            } else {
                Some("Nothing under this group runs.".to_string())
            }
        });

        let claimed = ordered
            .iter()
            .map(|c| row_of(c.id()))
            .find(|r| r.stop_job_id.is_some());
        let in_flight = status.key == "running" || status.key == "queued";
        let last_synced = if dropped.is_some() {
            None
        } else {
            group::group_last_synced(
                &ordered
                    .iter()
                    .map(|c| ChildStamp {
                        is_ingest: row_of(c.id()).phase == Phase::Ingest
                            && c.kind() == ChildKind::Step,
                        at: row_of(c.id()).status.at.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        };

        ManageRow {
            id: g.id.clone(),
            key: group::group_row_key(&g.id),
            path: vec![group::group_row_key(&g.id)],
            kind: RowKind::Group,
            group: None,
            written_group: None,
            inputs: vec![],
            phase: Phase::Other,
            function: None,
            r#type: g.r#type.clone(),
            name: g.name.clone().unwrap_or_else(|| g.id.clone()),
            params: serde_json::Value::Object(Default::default()),
            dropped: dropped.cloned(),
            status,
            status_from,
            last_synced,
            segments: (in_flight && !steps.is_empty()).then(|| {
                steps
                    .iter()
                    .map(|c| Segment {
                        id: c.id().to_string(),
                        key: row_of(c.id()).status.key.clone(),
                        label: row_of(c.id()).status.label.clone(),
                    })
                    .collect()
            }),
            seeds,
            run_blocked,
            reveal_blocked: (!on_disk).then(|| {
                "Nothing on disk yet \u{2014} this group hasn't produced anything.".to_string()
            }),
            stop_job_id: claimed.and_then(|r| r.stop_job_id.clone()),
            stop_target: claimed.map(|r| r.id.clone()),
            stop_label: claimed.and_then(|r| r.stop_label.clone()),
            stop_blocked: claimed.and_then(|r| r.stop_blocked.clone()),
            progress: ordered
                .iter()
                .map(|c| row_of(c.id()))
                .find(|r| r.status.key == "running")
                .and_then(|r| r.progress.clone()),
            // A group's log is a child's; `status_from` names which.
            last_run_id: String::new(),
            live_run_id: None,
            bytes: on_disk.then(|| tree.map(|t| t.bytes).unwrap_or(0)),
            history: tree.map(|t| t.history.clone()).unwrap_or_default(),
            outputs: children
                .iter()
                .flat_map(|(_, r)| r.outputs.iter().cloned())
                .collect(),
            reveal_path: on_disk.then(|| tree.map(|t| t.abs.clone()).unwrap_or_default()),
        }
    }
}
