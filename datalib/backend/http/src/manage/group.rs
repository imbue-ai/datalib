//! What a group's row on the Manage screen says about the steps and
//! applets filed under it: the order they run in, the status the row
//! shows, the instant it calls "last synced", and the steps a sync of
//! the group starts at. The rules are the aggregation table in
//! docs/dev/plans/groups_and_functions.md. Nothing here does arithmetic
//! across children: a group's bytes come from its own measured series.

use super::status::{compare_stamps, StatusView};

/// The grid's row id for a group. An applet may share its group's id —
/// the `unified_index` applet sits under the `unified_index` group — so
/// a group row needs a key no entry can have.
pub fn group_row_key(group_id: &str) -> String {
    format!("group:{group_id}")
}

/// The two kinds of entry filed under a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    Step,
    Applet,
}

pub trait Child {
    fn id(&self) -> &str;
    fn kind(&self) -> ChildKind;
    fn inputs(&self) -> &[String];
}

/// A group's children in pipeline order: a step that reads a sibling
/// comes after it, ties keep config order, and applets — never
/// scheduled — trail the steps.
pub fn pipeline_order<T: Child + Clone>(children: &[T]) -> Vec<T> {
    let steps: Vec<&T> = children
        .iter()
        .filter(|c| c.kind() == ChildKind::Step)
        .collect();
    let applets = children.iter().filter(|c| c.kind() == ChildKind::Applet);
    let sibling_ids: Vec<&str> = steps.iter().map(|s| s.id()).collect();
    let mut placed: Vec<T> = Vec::with_capacity(children.len());
    let mut done: Vec<&str> = Vec::new();
    while placed.len() < steps.len() {
        let ready = steps.iter().find(|s| {
            !done.contains(&s.id())
                && s.inputs().iter().all(|input| {
                    !sibling_ids.contains(&input.as_str()) || done.contains(&input.as_str())
                })
        });
        // A cycle within a group is a config the loader refuses, but
        // this runs against every entry as written: fall back to config
        // order rather than spin.
        let next = ready
            .or_else(|| steps.iter().find(|s| !done.contains(&s.id())))
            .expect("fewer placed than steps, so one is left");
        done.push(next.id());
        placed.push((*next).clone());
    }
    placed.extend(applets.cloned());
    placed
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildStatus {
    pub id: String,
    pub kind: ChildKind,
    pub status: StatusView,
}

/// The status a group row shows, and which child it is read from.
/// Running if any child is running; failed if any child failed;
/// otherwise the last step in pipeline order — the one whose state says
/// how far the group's data got. A group with only applets reads its
/// last applet. `children` must already be in pipeline order.
pub fn group_status(children: &[ChildStatus]) -> Option<(StatusView, String)> {
    if let Some(running) = children.iter().find(|c| c.status.key == "running") {
        return Some(read(running));
    }
    if let Some(failed) = children.iter().find(|c| c.status.key == "failed") {
        return Some(read(failed));
    }
    let last_step = children.iter().rfind(|c| c.kind == ChildKind::Step);
    last_step.or(children.last()).map(read)
}

/// The child's view, with the child named in the detail so the group's
/// tooltip says where its word came from.
fn read(child: &ChildStatus) -> (StatusView, String) {
    let detail = match &child.status.detail {
        Some(d) => format!("{}: {d}", child.id),
        None => child.id.clone(),
    };
    let status = StatusView {
        detail: Some(detail),
        ..child.status.clone()
    };
    (status, child.id.clone())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildStamp {
    pub is_ingest: bool,
    pub at: Option<String>,
}

/// When a group last synced: its ingest step's instant, else the newest
/// any child reports. The ingest step is what "synced" means for a
/// source, so a render that ran later does not move the group's stamp.
pub fn group_last_synced(children: &[ChildStamp]) -> Option<String> {
    if let Some(ingest) = children.iter().find(|c| c.is_ingest) {
        return ingest.at.clone();
    }
    children
        .iter()
        .filter_map(|c| c.at.as_deref())
        .max_by(|a, b| compare_stamps(Some(a), Some(b)))
        .map(str::to_string)
}

/// The steps a sync of the group starts at: its steps with no declared
/// inputs, less any the loader dropped. `datalib-dag --sync` takes
/// exactly these, and everything downstream follows.
pub fn group_seeds<T: Child>(children: &[T], is_dropped: impl Fn(&T) -> bool) -> Vec<String> {
    children
        .iter()
        .filter(|c| c.kind() == ChildKind::Step && c.inputs().is_empty() && !is_dropped(c))
        .map(|c| c.id().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct C {
        id: String,
        kind: ChildKind,
        inputs: Vec<String>,
    }
    impl Child for C {
        fn id(&self) -> &str {
            &self.id
        }
        fn kind(&self) -> ChildKind {
            self.kind
        }
        fn inputs(&self) -> &[String] {
            &self.inputs
        }
    }
    fn step(id: &str, inputs: &[&str]) -> C {
        C {
            id: id.into(),
            kind: ChildKind::Step,
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn applet(id: &str) -> C {
        C {
            id: id.into(),
            kind: ChildKind::Applet,
            inputs: vec![],
        }
    }
    fn ids(v: &[C]) -> Vec<&str> {
        v.iter().map(|c| c.id.as_str()).collect()
    }

    fn view(key: &str, at: Option<&str>, detail: Option<&str>) -> StatusView {
        StatusView {
            key: key.into(),
            label: key.into(),
            at: at.map(str::to_string),
            detail: detail.map(str::to_string),
            ..Default::default()
        }
    }
    fn child(id: &str, key: &str, kind: ChildKind, at: Option<&str>) -> ChildStatus {
        ChildStatus {
            id: id.into(),
            kind,
            status: view(key, at, None),
        }
    }
    use ChildKind::{Applet, Step};

    /// The scaffold files the `unified_index` applet under the
    /// `unified_index` group; both are rows, and the grid keys rows by id.
    #[test]
    fn group_row_key_cannot_collide_with_an_applet_that_shares_the_groups_id() {
        assert_ne!(group_row_key("unified_index"), "unified_index");
    }

    #[test]
    fn pipeline_order_puts_a_step_after_the_sibling_it_reads_whatever_the_config_order() {
        let out = pipeline_order(&[
            step("s/render_markdown", &["s/ingest"]),
            step("s/ingest", &[]),
        ]);
        assert_eq!(ids(&out), ["s/ingest", "s/render_markdown"]);
    }

    #[test]
    fn pipeline_order_keeps_config_order_between_steps_that_do_not_read_each_other() {
        let out = pipeline_order(&[
            step("u/grid_index", &["a/render_markdown"]),
            step("u/qmd_index", &["a/render_markdown"]),
        ]);
        assert_eq!(ids(&out), ["u/grid_index", "u/qmd_index"]);
    }

    #[test]
    fn pipeline_order_trails_the_applets_which_are_never_scheduled() {
        let out = pipeline_order(&[
            applet("u"),
            step("u/qmd_index", &[]),
            step("u/grid_index", &[]),
        ]);
        assert_eq!(ids(&out), ["u/qmd_index", "u/grid_index", "u"]);
    }

    #[test]
    fn pipeline_order_does_not_spin_on_a_cycle() {
        let out = pipeline_order(&[step("s/a", &["s/b"]), step("s/b", &["s/a"])]);
        assert_eq!(ids(&out), ["s/a", "s/b"]);
    }

    #[test]
    fn group_status_is_empty_for_a_group_with_nothing_under_it() {
        assert_eq!(group_status(&[]), None);
    }

    #[test]
    fn group_status_is_running_while_any_child_runs_whichever_it_is() {
        let got = group_status(&[
            child("s/ingest", "succeeded", Step, None),
            child("s/render_markdown", "running", Step, None),
        ])
        .unwrap();
        assert_eq!(got.0.key, "running");
        assert_eq!(got.1, "s/render_markdown");
    }

    #[test]
    fn group_status_is_failed_when_any_child_failed_even_if_a_later_one_is_up_to_date() {
        let got = group_status(&[
            child("s/ingest", "failed", Step, None),
            child("s/render_markdown", "skipped_up_to_date", Step, None),
        ])
        .unwrap();
        assert_eq!(got.0.key, "failed");
        assert_eq!(got.1, "s/ingest");
    }

    /// The fetch succeeded and the render is still queued: the group
    /// has not finished, and the last step is what says so.
    #[test]
    fn group_status_otherwise_reads_the_last_step_in_pipeline_order() {
        let got = group_status(&[
            child("s/ingest", "succeeded", Step, None),
            child("s/render_markdown", "queued", Step, None),
        ])
        .unwrap();
        assert_eq!(got.0.key, "queued");
        assert_eq!(got.1, "s/render_markdown");
    }

    #[test]
    fn group_status_reads_the_last_step_not_a_trailing_applet() {
        let got = group_status(&[
            child("u/grid_index", "succeeded", Step, None),
            child("u/qmd_index", "skipped_up_to_date", Step, None),
            child("u", "succeeded", Applet, None),
        ])
        .unwrap();
        assert_eq!(got.1, "u/qmd_index");
    }

    #[test]
    fn group_status_falls_back_to_an_applet_when_the_group_has_only_applets() {
        let got = group_status(&[child("view", "succeeded", Applet, None)]).unwrap();
        assert_eq!(got.1, "view");
    }

    /// The group row is the only place an applet's health shows while
    /// the group is folded.
    #[test]
    fn group_status_counts_an_applet_that_failed_to_start_as_a_failure() {
        let got = group_status(&[
            child("s/ingest", "succeeded", Step, None),
            child("s/render_markdown", "succeeded", Step, None),
            child("s_view", "failed", Applet, None),
        ])
        .unwrap();
        assert_eq!(got.0.key, "failed");
        assert_eq!(got.1, "s_view");
    }

    #[test]
    fn group_status_names_the_child_in_the_detail() {
        let with_detail = ChildStatus {
            id: "s/ingest".into(),
            kind: Step,
            status: view("failed", None, Some("boom")),
        };
        assert_eq!(
            group_status(&[with_detail]).unwrap().0.detail.as_deref(),
            Some("s/ingest: boom")
        );
        let plain = child("s/ingest", "succeeded", Step, None);
        assert_eq!(
            group_status(&[plain]).unwrap().0.detail.as_deref(),
            Some("s/ingest")
        );
    }

    #[test]
    fn group_status_keeps_the_childs_instant_which_feeds_last_synced() {
        let got = group_status(&[child(
            "s/ingest",
            "succeeded",
            Step,
            Some("2026-09-10T10:00:00+02:00"),
        )])
        .unwrap();
        assert_eq!(got.0.at.as_deref(), Some("2026-09-10T10:00:00+02:00"));
    }

    fn stamp(is_ingest: bool, at: Option<&str>) -> ChildStamp {
        ChildStamp {
            is_ingest,
            at: at.map(str::to_string),
        }
    }

    #[test]
    fn group_last_synced_is_the_ingest_steps_instant_even_when_the_render_ran_later() {
        let got = group_last_synced(&[
            stamp(true, Some("2026-09-10T10:00:00+02:00")),
            stamp(false, Some("2026-09-10T10:05:00+02:00")),
        ]);
        assert_eq!(got.as_deref(), Some("2026-09-10T10:00:00+02:00"));
    }

    #[test]
    fn group_last_synced_is_none_while_the_ingest_step_has_never_run_whatever_the_render_says() {
        let got = group_last_synced(&[
            stamp(true, None),
            stamp(false, Some("2026-09-10T10:05:00+02:00")),
        ]);
        assert_eq!(got, None);
    }

    /// Stamps in different offsets: the comparison is on the instant.
    #[test]
    fn group_last_synced_is_the_newest_childs_instant_for_a_group_with_no_ingest_step() {
        let got = group_last_synced(&[
            stamp(false, Some("2026-09-10T10:00:00+02:00")),
            stamp(false, Some("2026-09-10T09:30:00+00:00")),
            stamp(false, None),
        ]);
        assert_eq!(got.as_deref(), Some("2026-09-10T09:30:00+00:00"));
    }

    #[test]
    fn group_last_synced_is_none_when_nothing_has_run() {
        assert_eq!(group_last_synced(&[stamp(false, None)]), None);
    }

    #[test]
    fn group_seeds_is_the_groups_steps_with_no_inputs() {
        let got = group_seeds(
            &[
                step("s/ingest", &[]),
                step("s/render_markdown", &["s/ingest"]),
                applet("v"),
            ],
            |_| false,
        );
        assert_eq!(got, ["s/ingest"]);
    }

    #[test]
    fn group_seeds_leaves_out_a_step_the_loader_dropped() {
        let got = group_seeds(&[step("s/ingest", &[])], |c| c.id == "s/ingest");
        assert!(got.is_empty());
    }

    #[test]
    fn group_seeds_is_empty_for_a_group_whose_steps_all_read_something() {
        let got = group_seeds(&[step("u/grid_index", &["a/render_markdown"])], |_| false);
        assert!(got.is_empty());
    }
}
