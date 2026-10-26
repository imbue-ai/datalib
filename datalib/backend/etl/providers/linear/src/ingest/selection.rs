//! Which part of the workspace a run mirrors, resolved from the
//! config's team and project names against the workspace's own lists,
//! and the filter each walked connection is asked with.
//!
//! An issue is in when its team is chosen or its project is. The
//! projects mirrored are the chosen ones and every project a chosen team
//! works in, and a document, project update or milestone is in when its
//! project is. A comment follows what it is on.

use std::collections::BTreeSet;

use datalib_etl::download_problems::DownloadProblem;
use datalib_etl_linear_config::LinearSync;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    Some {
        teams: BTreeSet<String>,
        /// The projects named in the config, whose issues are in
        /// whatever their team.
        named_projects: BTreeSet<String>,
        /// `named_projects` and every project a chosen team works in.
        projects: BTreeSet<String>,
    },
}

fn id_of(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The ids under `v[key].nodes`, for a connection asked for as
/// `key { nodes { id } }`.
fn node_ids<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    v.get(key)
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(id_of)
}

/// Resolves `sync` against the workspace's teams and projects. A name
/// that matches nothing is a problem row and selects nothing.
pub fn resolve(
    sync: &LinearSync,
    teams: &[Value],
    projects: &[Value],
) -> (Selection, Vec<DownloadProblem>) {
    if sync.whole_workspace() {
        return (Selection::All, Vec::new());
    }
    let mut problems = Vec::new();
    let mut chosen_teams = BTreeSet::new();
    for want in &sync.teams {
        let want_trim = want.trim();
        let hits: Vec<&str> = teams
            .iter()
            .filter(|t| {
                str_of(t, "key").is_some_and(|k| k.eq_ignore_ascii_case(want_trim))
                    || str_of(t, "name") == Some(want_trim)
                    || id_of(t) == Some(want_trim)
            })
            .filter_map(id_of)
            .collect();
        if hits.is_empty() {
            problems.push(DownloadProblem::not_found(
                "teams",
                want,
                "no team in this workspace has that key or name",
            ));
        }
        chosen_teams.extend(hits.into_iter().map(String::from));
    }
    let mut named = BTreeSet::new();
    for want in &sync.projects {
        let want_trim = want.trim();
        let hits: Vec<&str> = projects
            .iter()
            .filter(|p| {
                str_of(p, "name") == Some(want_trim)
                    || str_of(p, "slugId") == Some(want_trim)
                    || id_of(p) == Some(want_trim)
            })
            .filter_map(id_of)
            .collect();
        if hits.is_empty() {
            problems.push(DownloadProblem::not_found(
                "projects",
                want,
                "no project in this workspace has that name",
            ));
        }
        named.extend(hits.into_iter().map(String::from));
    }
    let mut all_projects = named.clone();
    for p in projects {
        if node_ids(p, "teams").any(|t| chosen_teams.contains(t)) {
            all_projects.extend(id_of(p).map(String::from));
        }
    }
    (
        Selection::Some {
            teams: chosen_teams,
            named_projects: named,
            projects: all_projects,
        },
        problems,
    )
}

fn ids(set: &BTreeSet<String>) -> Value {
    json!({ "id": { "in": set.iter().collect::<Vec<_>>() } })
}

impl Selection {
    /// Names this selection in a `coverage` scope. What a walk has
    /// covered is covered for these teams and projects only, so a
    /// changed choice walks again from the top; records it already holds
    /// at their version are listed but not rewritten.
    pub fn key(&self) -> String {
        match self {
            Selection::All => "all".to_string(),
            Selection::Some {
                teams,
                named_projects,
                ..
            } => {
                let mut h = blake3::Hasher::new();
                for t in teams {
                    h.update(b"team:");
                    h.update(t.as_bytes());
                    h.update(b"\n");
                }
                for p in named_projects {
                    h.update(b"project:");
                    h.update(p.as_bytes());
                    h.update(b"\n");
                }
                h.finalize().to_hex()[..16].to_string()
            }
        }
    }

    pub fn issues(&self) -> Option<Value> {
        match self {
            Selection::All => None,
            Selection::Some {
                teams,
                named_projects,
                ..
            } => Some(json!({ "or": [
                { "team": ids(teams) },
                { "project": ids(named_projects) },
            ]})),
        }
    }

    pub fn comments(&self) -> Option<Value> {
        match self {
            Selection::All => None,
            Selection::Some {
                teams,
                named_projects,
                projects,
            } => Some(json!({ "or": [
                { "issue": { "team": ids(teams) } },
                { "issue": { "project": ids(named_projects) } },
                { "projectUpdate": { "project": ids(projects) } },
                { "project": ids(projects) },
            ]})),
        }
    }

    pub fn documents(&self) -> Option<Value> {
        match self {
            Selection::All => None,
            Selection::Some {
                teams, projects, ..
            } => Some(json!({ "or": [
                { "team": ids(teams) },
                { "project": ids(projects) },
            ]})),
        }
    }

    pub fn project_updates(&self) -> Option<Value> {
        match self {
            Selection::All => None,
            Selection::Some { projects, .. } => Some(json!({ "project": ids(projects) })),
        }
    }

    pub fn keeps_project(&self, project: &Value) -> bool {
        match self {
            Selection::All => true,
            Selection::Some { projects, .. } => {
                id_of(project).is_some_and(|p| projects.contains(p))
            }
        }
    }

    pub fn keeps_cycle(&self, cycle: &Value) -> bool {
        match self {
            Selection::All => true,
            Selection::Some { teams, .. } => cycle
                .get("team")
                .and_then(id_of)
                .is_some_and(|t| teams.contains(t)),
        }
    }

    pub fn keeps_milestone(&self, milestone: &Value) -> bool {
        match self {
            Selection::All => true,
            Selection::Some { projects, .. } => milestone
                .get("project")
                .and_then(id_of)
                .is_some_and(|p| projects.contains(p)),
        }
    }

    /// Linear cannot filter attachments by their issue's team, so the
    /// walk reads them all and keeps these.
    pub fn keeps_attachment(&self, attachment: &Value) -> bool {
        match self {
            Selection::All => true,
            Selection::Some {
                teams,
                named_projects,
                ..
            } => {
                let issue = attachment.get("issue");
                let team = issue.and_then(|i| i.get("team")).and_then(id_of);
                let project = issue.and_then(|i| i.get("project")).and_then(id_of);
                team.is_some_and(|t| teams.contains(t))
                    || project.is_some_and(|p| named_projects.contains(p))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn teams() -> Vec<Value> {
        vec![
            json!({"id": "t-eng", "key": "ENG", "name": "Engineering"}),
            json!({"id": "t-sci", "key": "SCI", "name": "Science"}),
        ]
    }

    fn projects() -> Vec<Value> {
        vec![
            json!({"id": "p-warp", "name": "Warp core refit", "slugId": "a1",
                   "teams": {"nodes": [{"id": "t-eng"}]}}),
            json!({"id": "p-probe", "name": "Probe survey", "slugId": "b2",
                   "teams": {"nodes": [{"id": "t-sci"}]}}),
            json!({"id": "p-shared", "name": "Shared", "slugId": "c3",
                   "teams": {"nodes": [{"id": "t-sci"}, {"id": "t-eng"}]}}),
        ]
    }

    fn sync(teams: &[&str], projects: &[&str]) -> LinearSync {
        LinearSync {
            teams: teams.iter().map(|s| s.to_string()).collect(),
            projects: projects.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn nothing_named_is_the_whole_workspace_and_filters_nothing() {
        let (sel, problems) = resolve(&sync(&[], &[]), &teams(), &projects());
        assert_eq!(sel, Selection::All);
        assert!(problems.is_empty());
        assert_eq!(sel.issues(), None);
        assert_eq!(sel.key(), "all");
        assert!(sel.keeps_attachment(&json!({})));
    }

    /// A team brings the projects it works in; a named project brings
    /// its issues whatever their team.
    #[test]
    fn a_team_brings_its_projects_and_a_project_its_issues() {
        let (sel, problems) = resolve(&sync(&["eng"], &["Probe survey"]), &teams(), &projects());
        assert!(problems.is_empty(), "{problems:?}");
        let Selection::Some {
            teams,
            named_projects,
            projects: all,
        } = &sel
        else {
            panic!("{sel:?}");
        };
        assert_eq!(teams.iter().collect::<Vec<_>>(), ["t-eng"]);
        assert_eq!(named_projects.iter().collect::<Vec<_>>(), ["p-probe"]);
        assert_eq!(
            all.iter().collect::<Vec<_>>(),
            ["p-probe", "p-shared", "p-warp"]
        );
        assert_eq!(
            sel.issues().unwrap(),
            json!({"or": [{"team": {"id": {"in": ["t-eng"]}}},
                          {"project": {"id": {"in": ["p-probe"]}}}]})
        );
        assert!(
            sel.keeps_attachment(&json!({"issue": {"team": {"id": "t-sci"},
            "project": {"id": "p-probe"}}}))
        );
        assert!(
            !sel.keeps_attachment(&json!({"issue": {"team": {"id": "t-sci"},
            "project": null}}))
        );
        assert!(sel.keeps_cycle(&json!({"team": {"id": "t-eng"}})));
        assert!(!sel.keeps_cycle(&json!({"team": {"id": "t-sci"}})));
    }

    #[test]
    fn a_name_matching_nothing_is_a_problem_row() {
        let (_, problems) = resolve(&sync(&["MED"], &["Genesis"]), &teams(), &projects());
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    /// The scope is the choice, not its spelling: a team named by key or
    /// by name is the same coverage.
    #[test]
    fn the_key_follows_the_resolved_ids() {
        let by_key = resolve(&sync(&["ENG"], &[]), &teams(), &projects()).0;
        let by_name = resolve(&sync(&["Engineering"], &[]), &teams(), &projects()).0;
        let other = resolve(&sync(&["SCI"], &[]), &teams(), &projects()).0;
        assert_eq!(by_key.key(), by_name.key());
        assert_ne!(by_key.key(), other.key());
        assert_eq!(by_key.key().len(), 16);
    }
}
