//! The raw store: one `(id, payload)` table per kind of Linear object,
//! keyed by Linear's id, the payload the object as the API answered it.
//! What each kind gives up to the sidecar (`<table>_bookkeeping.
//! volatile_payload`) is beside it: a field that moves without the
//! record being edited. `updatedAt` is one everywhere: a comment, a
//! board drag or a sub-issue moves it, and on the walked tables it is
//! the version the record is held at (`held_version`) anyway.

use std::marker::PhantomData;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, Migration, VolatilePath};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// One kind of object: its table and what it gives up to the sidecar.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub table: &'static str,
    pub volatile: &'static [VolatilePath<'static>],
}

/// The series Linear recomputes from a project's or a cycle's issues,
/// weekly or on any change to one of them, without moving the record's
/// own `updatedAt`.
macro_rules! progress_series {
    () => {
        [
            &["progress"] as VolatilePath<'static>,
            &["progressHistory"],
            &["currentProgress"],
            &["issueCountHistory"],
            &["completedIssueCountHistory"],
            &["scopeHistory"],
            &["completedScopeHistory"],
            &["inProgressScopeHistory"],
        ]
    };
}

const fn concat<const A: usize, const B: usize, const N: usize>(
    a: [VolatilePath<'static>; A],
    b: [VolatilePath<'static>; B],
) -> [VolatilePath<'static>; N] {
    let mut out: [VolatilePath<'static>; N] = [&[]; N];
    let mut i = 0;
    while i < A {
        out[i] = a[i];
        i += 1;
    }
    while i < N {
        out[i] = b[i - A];
        i += 1;
    }
    out
}

const UPDATED: VolatilePath<'static> = &["updatedAt"];

pub const TEAMS: Kind = Kind {
    table: "teams",
    volatile: &[
        UPDATED,
        &["issueCount"],
        &["progressHistory"],
        &["currentProgress"],
        &["activeCycle"],
    ],
};

/// A person's status and when they were last seen change as they work,
/// and say nothing about the account.
pub const USERS: Kind = Kind {
    table: "users",
    volatile: &[
        UPDATED,
        &["lastSeen"],
        &["statusEmoji"],
        &["statusLabel"],
        &["statusUntilAt"],
        &["createdIssueCount"],
    ],
};

pub const WORKFLOW_STATES: Kind = Kind {
    table: "workflow_states",
    volatile: &[UPDATED],
};

pub const ISSUE_LABELS: Kind = Kind {
    table: "issue_labels",
    volatile: &[UPDATED, &["lastAppliedAt"]],
};

pub const PROJECT_STATUSES: Kind = Kind {
    table: "project_statuses",
    volatile: &[UPDATED],
};

const CYCLE_VOLATILE: [VolatilePath<'static>; 9] = concat([UPDATED], progress_series!());
pub const CYCLES: Kind = Kind {
    table: "cycles",
    volatile: &CYCLE_VOLATILE,
};

/// A sort order moves when anyone drags a neighbour on a board.
const PROJECT_VOLATILE: [VolatilePath<'static>; 14] = concat(
    [
        UPDATED,
        &["sortOrder"],
        &["prioritySortOrder"],
        &["scope"],
        &["lastUpdate"],
        &["resourceCount"],
    ],
    progress_series!(),
);
pub const PROJECTS: Kind = Kind {
    table: "projects",
    volatile: &PROJECT_VOLATILE,
};

pub const PROJECT_MILESTONES: Kind = Kind {
    table: "project_milestones",
    volatile: &[
        UPDATED,
        &["sortOrder"],
        &["progress"],
        &["progressHistory"],
        &["currentProgress"],
    ],
};

pub const INITIATIVES: Kind = Kind {
    table: "initiatives",
    volatile: &[
        UPDATED,
        &["sortOrder"],
        &["prioritySortOrder"],
        &["lastUpdate"],
    ],
};

/// `customerTicketCount` is counted from the customer requests linked
/// to the issue, not written to it.
pub const ISSUES: Kind = Kind {
    table: "issues",
    volatile: &[
        UPDATED,
        &["sortOrder"],
        &["prioritySortOrder"],
        &["subIssueSortOrder"],
        &["customerTicketCount"],
    ],
};

pub const ISSUE_HISTORY: Kind = Kind {
    table: "issue_history",
    volatile: &[UPDATED],
};

/// `threadSummary` is Linear's AI summary of the thread, rewritten as
/// replies come.
pub const COMMENTS: Kind = Kind {
    table: "comments",
    volatile: &[UPDATED, &["threadSummary"]],
};

pub const DOCUMENTS: Kind = Kind {
    table: "documents",
    volatile: &[UPDATED, &["sortOrder"], &["subDocumentSortOrder"]],
};

/// `isStale` turns true when enough time has passed since the update.
pub const PROJECT_UPDATES: Kind = Kind {
    table: "project_updates",
    volatile: &[UPDATED, &["isStale"], &["commentCount"]],
};

pub const ATTACHMENTS: Kind = Kind {
    table: "attachments",
    volatile: &[UPDATED],
};

pub const KINDS: &[Kind] = &[
    TEAMS,
    USERS,
    WORKFLOW_STATES,
    ISSUE_LABELS,
    PROJECT_STATUSES,
    CYCLES,
    PROJECTS,
    PROJECT_MILESTONES,
    INITIATIVES,
    ISSUES,
    ISSUE_HISTORY,
    COMMENTS,
    DOCUMENTS,
    PROJECT_UPDATES,
    ATTACHMENTS,
];

/// One row of a `Kind`'s table, written through the shared bulk upsert.
/// `T` names the table at the type level, which the upsert needs.
pub struct Row<T> {
    pub id: String,
    pub payload: String,
    table: PhantomData<fn() -> T>,
}

impl<T> Row<T> {
    pub fn new(id: String, payload: String) -> Self {
        Self {
            id,
            payload,
            table: PhantomData,
        }
    }
}

pub trait Table {
    const KIND: Kind;
}

impl<T: Table> BulkUpsertable for Row<T> {
    const TABLE: &'static str = T::KIND.table;
    const TYPED_COLUMNS: &'static [&'static str] = &[];

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id).bind(&self.payload)
    }
}

macro_rules! tables {
    ($($marker:ident => $kind:ident),* $(,)?) => {
        $(
            pub enum $marker {}
            impl Table for $marker {
                const KIND: Kind = $kind;
            }
        )*
    };
}

tables! {
    TeamsT => TEAMS,
    UsersT => USERS,
    WorkflowStatesT => WORKFLOW_STATES,
    IssueLabelsT => ISSUE_LABELS,
    ProjectStatusesT => PROJECT_STATUSES,
    CyclesT => CYCLES,
    ProjectsT => PROJECTS,
    ProjectMilestonesT => PROJECT_MILESTONES,
    InitiativesT => INITIATIVES,
    IssuesT => ISSUES,
    IssueHistoryT => ISSUE_HISTORY,
    CommentsT => COMMENTS,
    DocumentsT => DOCUMENTS,
    ProjectUpdatesT => PROJECT_UPDATES,
    AttachmentsT => ATTACHMENTS,
}

pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = KINDS
        .iter()
        .map(|k| dr::wire_payload_table_ddl(k.table, &[]))
        .collect();
    out.push(datalib_etl_web::coverage::DDL.to_string());
    out.extend(KINDS.iter().map(|k| dr::bookkeeping_ddl_for(k.table)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_has_a_table_of_its_own() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.table).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len());
        assert_eq!(<Row<IssuesT> as BulkUpsertable>::TABLE, "issues");
        assert_eq!(<Row<AttachmentsT> as BulkUpsertable>::TABLE, "attachments");
    }

    /// The series concatenated into the project and cycle lists are all
    /// there, none left as the empty placeholder.
    #[test]
    fn no_volatile_path_is_empty() {
        for k in KINDS {
            for p in k.volatile {
                assert!(!p.is_empty(), "{}: an empty volatile path", k.table);
            }
        }
        assert!(PROJECTS.volatile.contains(&&["inProgressScopeHistory"][..]));
        assert!(CYCLES.volatile.contains(&&["progress"][..]));
    }

    /// A project nobody has touched in weeks still gains a point of
    /// `issueCountHistory` every week, with its `updatedAt` unmoved
    /// (measured on a real workspace). In the content payload that would
    /// read as an edit every week.
    #[test]
    fn a_projects_weekly_series_does_not_reach_the_content_payload() {
        let week = |n: usize| {
            json!({"id": "p1", "name": "Warp core refit", "updatedAt": "2026-08-01T00:00:00.000Z",
                   "issueCountHistory": vec![3.0; n], "progress": 0.1 * n as f64})
        };
        let (a, a_vol) = dr::split_volatile(&week(4), PROJECTS.volatile);
        let (b, b_vol) = dr::split_volatile(&week(5), PROJECTS.volatile);
        assert_eq!(a, b);
        assert_ne!(a_vol, b_vol);
        assert_eq!(b["name"], "Warp core refit");
    }
}
