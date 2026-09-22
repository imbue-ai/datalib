//! `GET /api/manage/rows`: the Manage screen's tree, assembled. One row
//! per entry in the config *file* — a group with its steps and applets
//! under it — plus one for `system/`, which no config names but which
//! is on disk like the rest; with the status, timestamps, sizes and
//! actions the screen draws, joined here from the config, the runner's
//! record, the run store, the job queue, the usage sampler and the
//! applet supervisor, and typed by the columns the response declares.
//! What stays in the browser is what needs the wizard's descriptors:
//! whether the form can edit a row, what Browse opens, and an ingest
//! step's Download/Import label.

mod activity;
mod group;
mod problems;
mod status;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Query, State};
use axum::Json;
use datalib_columns::{
    source_catalog, Action, Chip, ColumnSpec, ColumnType, Identity, Sample, Segment, Timeseries,
};
use datalib_dag::written::{WrittenApplet, WrittenEntries, WrittenGroup, WrittenStep};
use datalib_dag::{Diagnostic, EntryKind, Severity};
use serde::{Deserialize, Serialize};

use crate::usage::OutputStorage;
use crate::{usage, AppState, DagRecord, DagRunInfo};
use group::{Child, ChildKind, ChildStamp, ChildStatus};
use status::{EffectiveRun, StatusArgs, StatusFloor, StatusView, StepEdges, StepRecord};

pub use problems::{counts_by_step, ProblemCounts};
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
    /// `system/`: the run log and the app's own stores. Not a config
    /// entry — nothing syncs, edits or removes it — but it takes disk
    /// and its log is browsable.
    System,
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

    /// The word behind the step-role glyph, and the glyph's token.
    fn label(self) -> &'static str {
        match self {
            Phase::Ingest => "Ingest",
            Phase::Render => "Render",
            Phase::Index => "Index",
            Phase::Other => "Step",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Phase::Ingest => "step:ingest",
            Phase::Render => "step:render",
            Phase::Index => "step:index",
            Phase::Other => "step:other",
        }
    }
}

/// The columns the rows carry, in the order the screen shows them.
pub fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("name", "Name", ColumnType::Identity)
            .describe("What the config calls it; its id — the folder under the data root — beside it when they differ.")
            .editable(),
        ColumnSpec::new("actions", "Actions", ColumnType::Actions)
            .describe("Browse this row's data, and sync it \u{2014} or stop the sync in progress."),
        ColumnSpec::new("type", "Type", ColumnType::Identity)
            .describe("The service this source mirrors."),
        ColumnSpec::new("status", "Status", ColumnType::Status)
            .describe("What it is doing now, or did last. Hover for why; double-click for the log."),
        ColumnSpec::new("activity", "Activity", ColumnType::Chips)
            .describe("What a running step has reported: what is queued ahead of it, what it has counted, and how fast."),
        ColumnSpec::new("problems", "Problems", ColumnType::Chips)
            .describe("Errors (records dropped) and warnings (records kept with something lost) the step's store holds, as of its last run. A green zero means it counted and found none; blank means it has never counted. Double-click for the list."),
        ColumnSpec::new("last_synced", "Last synced", ColumnType::Timestamp),
        ColumnSpec::new("disk", "Bytes on disk", ColumnType::Timeseries)
            .describe("What this tree weighs, with the last few minutes behind it."),
    ]
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
    /// A step's `params`, as JSON. `{}` off a step.
    pub params: serde_json::Value,
    /// The Name column: the label the config gives it and the glyph for
    /// its role. Under a group a step is labelled by what it does there
    /// ("Render markdown"); the browser reads an ingest step's
    /// "Download" / "Import" off `params` against what its provider
    /// declares, and overrides that one label.
    pub name: Identity,
    /// The Type column: the source type, resolved. None for a group
    /// that mirrors nothing and the steps under it.
    pub r#type: Option<Identity>,
    /// The loader's reason this entry is not in the pipeline, or null
    /// if it is. A dropped entry still has a row — it is still in the
    /// file, and the file is what the user edits.
    pub dropped: Option<Diagnostic>,
    pub status: StatusView,
    /// For a group row, the child whose status it shows — the row a
    /// double-click on Status opens the log of.
    pub status_from: Option<String>,
    /// What the step has reported in the run in flight.
    pub activity: Vec<Chip>,
    /// The errors and warnings its store holds — see `manage::problems`.
    /// A group shows its render step's, the union for the source.
    pub problems: Vec<Chip>,
    pub last_synced: Option<String>,
    /// Bytes on disk, with the recent measurements behind the number.
    pub disk: Timeseries,
    /// The Sync column: a Sync button, or a Stop button while a job has
    /// this row claimed.
    pub actions: Vec<Action>,
    /// What a sync of this row starts at: the step itself, or for a
    /// group its steps with no inputs. Empty exactly when the sync
    /// action says why.
    pub seeds: Vec<String>,
    pub reveal_blocked: Option<String>,
    /// The active job that has claimed this step, when one has: what
    /// the Stop action cancels.
    pub stop_job_id: Option<String>,
    /// The run the step's `last_run` happened in — where its log is.
    /// Empty when it has never run, or ran before runs had ids.
    pub last_run_id: String,
    /// The run in flight, when this step is in it — where its live log
    /// is.
    pub live_run_id: Option<String>,
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
    pub columns: Vec<ColumnSpec>,
    /// The rows form a tree; each carries its `path`.
    pub tree: bool,
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
        usage::sample_on_demand(&s.usage, &s.app, s.root.clone(), &s.root_tx).await;
    }
    let config_path = s.config_path();
    let text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let record = crate::dag_record(&s.root).await;
    let storage = s
        .usage
        .snapshot(s.root.as_path(), &usage::measured_trees(&config_path))
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
                columns: columns(),
                tree: true,
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
        columns: columns(),
        tree: true,
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

fn browse_action(label: &str, blocked: Option<String>) -> Action {
    Action {
        id: "browse".into(),
        label: label.into(),
        enabled: blocked.is_none(),
        disabled_reason: blocked,
        danger: false,
    }
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
        rows.extend(self.system_rows());
        rows
    }

    /// The System group and its Logs child, after everything the
    /// config declares: `system/` as a whole, and the run store's
    /// directory inside it.
    fn system_rows(&self) -> [ManageRow; 2] {
        let dir = datalib_core::layout::SYSTEM_DIR;
        let log = datalib_core::layout::RUNS_DIR_REL;
        let dir_tree = self.outputs.iter().find(|o| o.path == dir);
        let log_tree = self.outputs.iter().find(|o| o.path == log);
        let dir_disk = dir_tree.filter(|t| t.present);
        let log_disk = log_tree.filter(|t| t.present);
        let sync = || Action {
            id: "sync".into(),
            label: "Sync now".into(),
            enabled: false,
            disabled_reason: Some(
                "Nothing here runs on its own \u{2014} every run writes to it.".to_string(),
            ),
            danger: false,
        };
        let row = |id: &str,
                   path: Vec<String>,
                   name: Identity,
                   disk: Timeseries,
                   browse: Action,
                   on_disk: Option<&OutputStorage>| ManageRow {
            id: id.to_string(),
            key: id.to_string(),
            path,
            kind: RowKind::System,
            group: None,
            written_group: None,
            inputs: vec![],
            phase: Phase::Other,
            function: None,
            params: serde_json::Value::Object(Default::default()),
            name,
            r#type: None,
            dropped: None,
            status: StatusView::default(),
            status_from: None,
            activity: vec![],
            problems: vec![],
            last_synced: None,
            disk,
            actions: vec![browse, sync()],
            seeds: vec![],
            reveal_blocked: on_disk
                .is_none()
                .then(|| "Nothing on disk yet.".to_string()),
            stop_job_id: None,
            last_run_id: String::new(),
            live_run_id: None,
            reveal_path: on_disk.map(|t| t.abs.clone()),
        };
        let group = row(
            dir,
            vec![dir.to_string()],
            Identity {
                id: dir.to_string(),
                label: "System".into(),
                icon: None,
                detail: Some("Group".into()),
            },
            Timeseries {
                value: dir_disk.map(|t| t.bytes as i64),
                unit: "bytes".into(),
                samples: dir_tree.map(samples).unwrap_or_default(),
                detail: Some(match dir_disk {
                    None => "Nothing on disk yet.".to_string(),
                    Some(t) => format!(
                        "{} in {dir}/ \u{2014} the run log {}, and the job queue, the usage \
                         samples and the feedback filed here.",
                        human_bytes(t.bytes),
                        human_bytes(log_disk.map_or(0, |l| l.bytes)),
                    ),
                }),
            },
            browse_action(
                "Browse the log",
                Some("The log under this group is what to browse.".to_string()),
            ),
            dir_disk,
        );
        let logs = row(
            log,
            vec![dir.to_string(), log.to_string()],
            Identity {
                id: log.to_string(),
                label: "Logs".into(),
                icon: None,
                detail: Some("Every run's step states and log lines".into()),
            },
            Timeseries {
                value: log_disk.map(|t| t.bytes as i64),
                unit: "bytes".into(),
                samples: log_tree.map(samples).unwrap_or_default(),
                detail: Some(match log_disk {
                    None => "Nothing on disk yet \u{2014} no run has been recorded.".to_string(),
                    Some(t) => format!(
                        "{} in {log}/ \u{2014} the store and the WAL beside it.",
                        human_bytes(t.bytes)
                    ),
                }),
            },
            browse_action("Browse the log", None),
            log_disk,
        );
        [group, logs]
    }
}

struct RowCtx<'a> {
    snap: &'a Snapshot<'a>,
    edges: &'a [StepEdges],
    claims: &'a HashMap<String, &'a app_schema::sync_jobs::SyncJobRow>,
    run: Option<&'a EffectiveRun>,
    stale: bool,
}

/// Base-10 units, matching what a file manager shows — the question
/// behind the column is "how much of my disk is this".
fn human_bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let units = ["kB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1000.0;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{} {}", v.round() as u64, units[i])
    }
}

fn samples(o: &OutputStorage) -> Vec<Sample> {
    o.history
        .iter()
        .map(|s| Sample {
            at: s.at.clone(),
            value: s.bytes as i64,
        })
        .collect()
}

/// The breakdown behind a size: per output, split into parts where the
/// backend found a split — entities vs attachments — which answers "why
/// is this so big" far more often than the total does.
fn breakdown(present: &[&OutputStorage]) -> String {
    present
        .iter()
        .map(|o| {
            if o.parts.is_empty() {
                format!("{}: {}", o.path, human_bytes(o.bytes))
            } else {
                let parts = o
                    .parts
                    .iter()
                    .map(|x| format!("{} {}", x.label, human_bytes(x.bytes)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: {parts}", o.path)
            }
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
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

    fn progress(&self, id: &str) -> Option<&crate::DagStepProgress> {
        self.snap.record.progress.get(id).filter(|_| !self.stale)
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
        let mut held = floor.hold(id, key, view);
        // The step's own words and how far along it is, while it runs.
        if let Some(p) = self.progress(id) {
            if let Some(msg) = &p.msg {
                held.detail = Some(msg.clone());
            }
            if held.key == "running" {
                held.fraction = activity::fraction(p);
            }
        }
        held
    }

    /// Sync, or Stop while a job has the row claimed: the one button
    /// beside Browse.
    fn sync_action(&self, id: &str, run_blocked: Option<String>) -> (Action, Option<String>) {
        if let Some(job) = self.claims.get(id) {
            let of = match job.source_ids.as_deref().filter(|s| !s.is_empty()) {
                Some(ids) => format!("the {} of {ids}", status::job_verb(job)),
                None => "the sync in progress".to_string(),
            };
            // Once asked to stop there is nothing more to ask: the steps
            // in flight are checkpointing, and the face says so until
            // they exit.
            let stopping = job.is_stopping();
            let label = if stopping {
                format!("Stopping {of}")
            } else {
                format!("Stop {of}")
            };
            let stop = Action {
                id: "stop".into(),
                enabled: !stopping,
                disabled_reason: stopping
                    .then(|| format!("{label} \u{2014} its steps are checkpointing and exiting.")),
                label,
                danger: true,
            };
            return (stop, Some(job.id.clone()));
        }
        let sync = Action {
            id: "sync".into(),
            label: "Sync now".into(),
            enabled: run_blocked.is_none(),
            disabled_reason: run_blocked,
            danger: false,
        };
        (sync, None)
    }

    fn entry_row(&self, e: &Entry<'_>, floor: &mut StatusFloor) -> ManageRow {
        let id = e.id().to_string();
        let dropped = self.dropped(&id, e.entry_kind());
        let dropped_why = dropped.map(not_in_pipeline);
        let group = self.declared_group(e.group());
        // A step writes exactly one tree, and it is the step's id.
        let tree = match e {
            Entry::Step(_) => self.snap.outputs.iter().find(|o| o.path == id),
            Entry::Applet(_) => None,
        };
        let on_disk = tree.filter(|o| o.present);

        let (status, run_blocked, seeds, last_run_id, live_run_id) = match e {
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
                (status, run_blocked, seeds, last_run_id, live_run_id)
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
                        detail: Some(err.clone()),
                        ..Default::default()
                    }
                } else {
                    StatusView {
                        key: "succeeded".into(),
                        label: "Up".into(),
                        detail: Some("The gateway has this applet up.".into()),
                        ..Default::default()
                    }
                };
                (
                    status,
                    Some(
                        "Applets aren't scheduled \u{2014} the server starts one when something asks for it."
                            .to_string(),
                    ),
                    vec![],
                    String::new(),
                    None,
                )
            }
        };

        let reveal_blocked = match e {
            Entry::Applet(_) => {
                Some("An applet owns no files \u{2014} it serves endpoints.".to_string())
            }
            Entry::Step(_) if on_disk.is_none() => {
                Some("Nothing on disk yet \u{2014} this hasn't produced anything.".to_string())
            }
            Entry::Step(_) => None,
        };

        let (name, params, function, phase, r#type) = match e {
            Entry::Step(s) => {
                let phase = Phase::of_function(s.function.as_deref());
                // Under a group the name is the group's; the step's
                // label says what it does there. At the top level the
                // step is its own thing and keeps the name the config
                // gave it.
                let label = if group.is_some() {
                    child_label(s)
                } else {
                    s.name.clone().unwrap_or_else(|| default_name(&id))
                };
                let r#type = s
                    .group
                    .as_deref()
                    .and_then(|g| self.snap.written.groups.iter().find(|x| x.id == g))
                    .and_then(|g| g.r#type.as_deref())
                    .map(|t| source_catalog::source_type(t, &s.params));
                let name = Identity {
                    id: id.clone(),
                    label,
                    icon: Some(phase.icon().into()),
                    detail: Some(phase.label().into()),
                };
                (name, s.params.clone(), s.function.clone(), phase, r#type)
            }
            Entry::Applet(a) => (
                Identity {
                    id: id.clone(),
                    label: default_name(&id),
                    icon: Some("applet".into()),
                    detail: Some("Applet".into()),
                },
                serde_json::Value::Object(Default::default()),
                None,
                Phase::Other,
                a.r#type
                    .as_deref()
                    .map(|t| source_catalog::source_type(t, &serde_json::Value::Null)),
            ),
        };

        let disk = match e {
            Entry::Applet(_) => Timeseries {
                detail: Some("An applet owns no artifacts.".into()),
                unit: "bytes".into(),
                ..Default::default()
            },
            Entry::Step(_) => Timeseries {
                value: on_disk.map(|o| o.bytes as i64),
                unit: "bytes".into(),
                samples: tree.map(samples).unwrap_or_default(),
                detail: Some(match on_disk {
                    None => {
                        "Nothing on disk yet \u{2014} this hasn't produced anything.".to_string()
                    }
                    Some(o) if o.parts.is_empty() => human_bytes(o.bytes),
                    Some(o) => format!("{} \u{2014} {}", human_bytes(o.bytes), breakdown(&[o])),
                }),
            },
        };

        let activity = match e {
            Entry::Step(_) => self.progress(&id).map(activity::chips).unwrap_or_default(),
            Entry::Applet(_) => vec![],
        };
        let problems = match e {
            Entry::Step(_) => problems::chips(self.snap.record.problems.get(&id)),
            Entry::Applet(_) => vec![],
        };
        // A source is browsed as one thing, from its group's row. A
        // step's rows are not a separate view of the data; they are the
        // same rows.
        let browse = browse_action(
            "Browse this data",
            Some(match e {
                Entry::Step(_) => "Browse a source from its group's row.".to_string(),
                Entry::Applet(_) => {
                    "An applet serves endpoints; it has no rows of its own.".to_string()
                }
            }),
        );
        let (sync, stop_job_id) = self.sync_action(&id, run_blocked);
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
            params,
            name,
            r#type,
            dropped: dropped.cloned(),
            last_synced: status.at.clone(),
            status,
            status_from: None,
            activity,
            problems,
            disk,
            actions: vec![browse, sync],
            seeds,
            reveal_blocked,
            stop_job_id,
            last_run_id,
            live_run_id,
            reveal_path: on_disk.map(|o| o.abs.clone()),
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
        let (mut status, status_from) = if let Some(d) = dropped {
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
        // A group with a run in flight: one segment per step, in
        // pipeline order, drawn as a bar instead of the glyph. No
        // arithmetic across children; the bar *is* the children.
        let in_flight = status.key == "running" || status.key == "queued";
        if in_flight && !steps.is_empty() {
            status.segments = Some(
                steps
                    .iter()
                    .map(|c| Segment {
                        id: c.id().to_string(),
                        key: row_of(c.id()).status.key.clone(),
                        label: row_of(c.id()).status.label.clone(),
                    })
                    .collect(),
            );
        }
        status.fraction = None;

        // The folder the group's steps write into, measured as a tree
        // of its own by the usage walker — not the sum of two series
        // sampled at different instants. It also counts anything else
        // in the folder, which is the right answer for "what does this
        // source weigh".
        let tree = self.snap.outputs.iter().find(|o| o.path == g.id);
        let on_disk = tree.filter(|t| t.present);
        let child_trees: Vec<&OutputStorage> = children
            .iter()
            .filter_map(|(e, _)| {
                self.snap
                    .outputs
                    .iter()
                    .find(|o| o.path == e.id() && o.present)
            })
            .collect();
        let disk = Timeseries {
            value: on_disk.map(|t| t.bytes as i64),
            unit: "bytes".into(),
            samples: tree.map(samples).unwrap_or_default(),
            detail: Some(match on_disk {
                None => {
                    "Nothing on disk yet \u{2014} this group hasn't produced anything.".to_string()
                }
                Some(t) => format!(
                    "{} in {}/ \u{2014} {}",
                    human_bytes(t.bytes),
                    g.id,
                    breakdown(&child_trees)
                ),
            }),
        };

        let is_dropped = |c: &Entry<'_>| row_of(c.id()).dropped.is_some();
        // A diff group has no source step of its own: a sync of it is a
        // sync of what its step reads, which is its source's ingest.
        let seeds = if g.r#type.as_deref() == Some(datalib_dag::config::DIFF_GROUP_TYPE) {
            group::diff_group_seeds(&ordered, is_dropped)
        } else {
            group::group_seeds(&ordered, is_dropped)
        };
        let run_blocked = dropped_why.clone().or_else(|| {
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
        // A group's rows reach the index through its `render_markdown`
        // step, so having one is exactly the condition for having
        // anything to browse. The index group has no type and no render
        // step: browsing it is the projection across every source.
        let browse = if g.r#type.is_none() {
            browse_action("Browse every source", dropped_why.clone())
        } else {
            let has_render = ordered
                .iter()
                .any(|c| c.kind() == ChildKind::Step && row_of(c.id()).phase == Phase::Render);
            browse_action(
                "Browse this data",
                dropped_why.clone().or_else(|| {
                    (!has_render).then(|| {
                        "This source has no render step, so none of what it downloads reaches \
                         the grid. Its files are on disk \u{2014} open the folder instead."
                            .to_string()
                    })
                }),
            )
        };
        // While a job has a child claimed, the group's button is that
        // child's Stop.
        let claimed = ordered
            .iter()
            .map(|c| c.id())
            .find(|id| row_of(id).stop_job_id.is_some());
        let (sync, stop_job_id) = self.sync_action(claimed.unwrap_or(&g.id), run_blocked);
        let activity = ordered
            .iter()
            .map(|c| row_of(c.id()))
            .find(|r| r.status.key == "running")
            .map(|r| r.activity.clone())
            .unwrap_or_default();
        // The last step in the pipeline that has counted: render's store
        // is the union of everything upstream of it for this source, and
        // the index's is the union of every source.
        let problems = ordered
            .iter()
            .rev()
            .map(|c| row_of(c.id()))
            .find(|r| !r.problems.is_empty())
            .map(|r| r.problems.clone())
            .unwrap_or_default();
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
            params: serde_json::Value::Object(Default::default()),
            name: Identity {
                id: g.id.clone(),
                label: g.name.clone().unwrap_or_else(|| g.id.clone()),
                icon: None,
                detail: Some("Group".into()),
            },
            // A group that mirrors nothing — the unified index — has no
            // type, and a blank cell is the honest mark for it.
            r#type: g.r#type.as_deref().map(|t| {
                let ingest = children
                    .iter()
                    .find_map(|(e, r)| match e {
                        Entry::Step(s) if r.phase == Phase::Ingest => Some(&s.params),
                        _ => None,
                    })
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                source_catalog::source_type(t, &ingest)
            }),
            dropped: dropped.cloned(),
            status,
            status_from,
            activity,
            problems,
            last_synced,
            disk,
            actions: vec![browse, sync],
            seeds,
            reveal_blocked: on_disk.is_none().then(|| {
                "Nothing on disk yet \u{2014} this group hasn't produced anything.".to_string()
            }),
            stop_job_id,
            // A group's log is a child's; `status_from` names which.
            last_run_id: String::new(),
            live_run_id: None,
            reveal_path: on_disk.map(|t| t.abs.clone()),
        }
    }
}
