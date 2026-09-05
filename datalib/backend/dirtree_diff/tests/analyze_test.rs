//! The diff interpretation, with no database and no HTML.
//!
//! Everything here drives `analyze`, the pure function between what was
//! read (`Inputs`) and what was concluded (`DiffResult`). A test builds
//! the rows a doltlite prolly diff would have produced and asserts on
//! the resulting statuses, so move pairing, subtree rollup, the
//! delete-vs-copy-remains distinction and in-tree duplicate grouping
//! are all pinned without a `.doltlite_db` or a browser in the picture.

use std::collections::BTreeMap;

use datalib_dirtree_diff::analyze;
use datalib_dirtree_diff::analyze::group_duplicates;
use datalib_dirtree_diff::model::{Diff, DiffResult, Entry, Inputs, Side, SideInput, Status};

// Digests are only ever compared for equality, so the tests use short
// readable stand-ins rather than real 64-char blake3 hex.
const ALPHA: &str = "AAAA";
const BETA: &str = "BBBB";
const GAMMA: &str = "CCCC";
const DELTA: &str = "DDDD";

fn file(path: &str, digest: &str) -> Entry {
    sized_file(path, digest, 10)
}

fn sized_file(path: &str, digest: &str, size: i64) -> Entry {
    Entry {
        path: path.into(),
        kind: "file".into(),
        size,
        digest: digest.into(),
    }
}

fn dir(path: &str, digest: &str) -> Entry {
    sized_dir(path, digest, 100)
}

fn sized_dir(path: &str, digest: &str, size: i64) -> Entry {
    Entry {
        path: path.into(),
        kind: "dir".into(),
        size,
        digest: digest.into(),
    }
}

#[derive(Default)]
struct Case {
    removed: Vec<Entry>,
    added: Vec<Entry>,
    modified: Vec<(Entry, Entry)>,
    copies_right: BTreeMap<String, String>,
    copies_left: BTreeMap<String, String>,
    left_full: Option<Vec<Entry>>,
    right_full: Option<Vec<Entry>>,
    left_dups: Vec<Entry>,
    right_dups: Vec<Entry>,
}

impl Case {
    fn run(self) -> DiffResult {
        analyze(&Inputs {
            left: SideInput {
                db: "/l.doltlite_db".into(),
                reference: "HEAD".into(),
                commit: "1".repeat(40),
                full: self.left_full,
                dup_candidates: self.left_dups,
            },
            right: SideInput {
                db: "/r.doltlite_db".into(),
                reference: "HEAD".into(),
                commit: "2".repeat(40),
                full: self.right_full,
                dup_candidates: self.right_dups,
            },
            diff: Diff {
                removed: self.removed,
                added: self.added,
                modified: self.modified,
            },
            copies_right: self.copies_right,
            copies_left: self.copies_left,
            dup_threshold: 0,
            copy_detection: true,
            unified: false,
        })
    }
}

fn statuses(r: &DiffResult, side: Side) -> Vec<(String, Status)> {
    r.statuses(side).into_iter().collect()
}

// ---------------------------------------------------------------------
// move detection
// ---------------------------------------------------------------------

#[test]
fn same_digest_at_a_new_path_is_a_move() {
    let r = Case {
        removed: vec![file("notes.txt", ALPHA)],
        added: vec![file("archive/notes.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("notes.txt".to_string(), Status::MovedOut)]
    );
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("archive/notes.txt".to_string(), Status::MovedIn)]
    );
    assert_eq!(
        r.node(Side::Left, "notes.txt").unwrap().peer.as_deref(),
        Some("archive/notes.txt")
    );
}

#[test]
fn a_different_digest_at_a_new_path_is_not_a_move() {
    let r = Case {
        removed: vec![file("notes.txt", ALPHA)],
        added: vec![file("archive/notes.txt", BETA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("notes.txt".to_string(), Status::Removed)]
    );
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("archive/notes.txt".to_string(), Status::Added)]
    );
}

#[test]
fn kind_must_match_too() {
    // A file and a directory sharing a digest are not a move.
    let r = Case {
        removed: vec![file("thing", ALPHA)],
        added: vec![dir("thing_dir", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("thing".to_string(), Status::Removed)]
    );
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("thing_dir".to_string(), Status::Added)]
    );
}

#[test]
fn pairing_prefers_the_candidate_that_kept_its_basename() {
    let r = Case {
        removed: vec![file("a/report.txt", ALPHA), file("a/memo.txt", ALPHA)],
        added: vec![file("b/memo.txt", ALPHA), file("b/report.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.node(Side::Left, "a/report.txt").unwrap().peer.as_deref(),
        Some("b/report.txt")
    );
    assert_eq!(
        r.node(Side::Left, "a/memo.txt").unwrap().peer.as_deref(),
        Some("b/memo.txt")
    );
}

// ---------------------------------------------------------------------
// subtree rollup
// ---------------------------------------------------------------------

fn moved_tree() -> DiffResult {
    Case {
        removed: vec![
            dir("docs", ALPHA),
            dir("docs/reports", BETA),
            file("docs/reports/q3.txt", GAMMA),
        ],
        added: vec![
            dir("archive", ALPHA),
            dir("archive/reports", BETA),
            file("archive/reports/q3.txt", GAMMA),
        ],
        ..Default::default()
    }
    .run()
}

#[test]
fn only_the_outermost_directory_is_reported() {
    let r = moved_tree();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("docs".to_string(), Status::MovedOut)]
    );
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("archive".to_string(), Status::MovedIn)]
    );
}

#[test]
fn the_survivor_counts_what_it_absorbed() {
    let r = moved_tree();
    assert_eq!(r.node(Side::Left, "docs").unwrap().rolled_up, 2);
    assert_eq!(r.summary.moves, 1);
    assert_eq!(r.summary.moved_entries, 3);
    assert_eq!(r.summary.rolled_up, 2);
}

#[test]
fn a_descendant_that_moved_somewhere_else_survives_the_rollup() {
    // Only entries making the *same* journey get absorbed.
    let r = Case {
        removed: vec![dir("docs", ALPHA), file("docs/stray.txt", DELTA)],
        added: vec![dir("archive", ALPHA), file("elsewhere/stray.txt", DELTA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![
            ("docs".to_string(), Status::MovedOut),
            ("docs/stray.txt".to_string(), Status::MovedOut),
        ]
    );
    assert_eq!(r.node(Side::Left, "docs").unwrap().rolled_up, 0);
}

// ---------------------------------------------------------------------
// deletes and copies
// ---------------------------------------------------------------------

#[test]
fn a_delete_with_no_surviving_copy() {
    let r = Case {
        removed: vec![file("gone.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("gone.txt".to_string(), Status::Removed)]
    );
    assert_eq!(r.summary.removed, 1);
}

#[test]
fn a_delete_whose_bytes_survive_elsewhere_on_the_right() {
    let r = Case {
        removed: vec![file("dup/copy.txt", ALPHA)],
        copies_right: [(ALPHA.to_string(), "keep/original.txt".to_string())].into(),
        ..Default::default()
    }
    .run();
    let node = r.node(Side::Left, "dup/copy.txt").unwrap();
    assert_eq!(node.status, Status::RemovedButCopyRemains);
    assert_eq!(node.peer.as_deref(), Some("keep/original.txt"));
    assert_eq!(r.summary.removed, 0);
    assert_eq!(r.summary.removed_but_copy_remains, 1);
}

#[test]
fn new_content_is_an_add() {
    let r = Case {
        added: vec![file("fresh.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("fresh.txt".to_string(), Status::Added)]
    );
}

#[test]
fn a_new_path_holding_pre_existing_bytes_is_a_copy() {
    let r = Case {
        added: vec![file("backup.txt", ALPHA)],
        copies_left: [(ALPHA.to_string(), "original.txt".to_string())].into(),
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("backup.txt".to_string(), Status::AddedFromCopy)]
    );
    assert_eq!(r.summary.added, 0);
    assert_eq!(r.summary.added_from_copy, 1);
}

#[test]
fn a_copied_directory_rolls_up() {
    let r = Case {
        added: vec![
            dir("themes/dark_backup", ALPHA),
            file("themes/dark_backup/base.css", BETA),
        ],
        copies_left: [
            (ALPHA.to_string(), "themes/dark".to_string()),
            (BETA.to_string(), "themes/dark/base.css".to_string()),
        ]
        .into(),
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("themes/dark_backup".to_string(), Status::AddedFromCopy)]
    );
    assert_eq!(
        r.node(Side::Right, "themes/dark_backup").unwrap().rolled_up,
        1
    );
}

#[test]
fn copy_detection_off_downgrades_to_a_plain_delete_and_add() {
    // Without the lookups, nothing may be *upgraded* to a copy.
    let r = Case {
        removed: vec![file("dup/copy.txt", ALPHA)],
        added: vec![file("backup.txt", BETA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("dup/copy.txt".to_string(), Status::Removed)]
    );
    assert_eq!(
        statuses(&r, Side::Right),
        vec![("backup.txt".to_string(), Status::Added)]
    );
}

// ---------------------------------------------------------------------
// modifications
// ---------------------------------------------------------------------

#[test]
fn a_changed_file_is_a_finding_on_both_sides() {
    let r = Case {
        modified: vec![(file("a.txt", ALPHA), file("a.txt", BETA))],
        ..Default::default()
    }
    .run();
    assert_eq!(
        statuses(&r, Side::Left),
        vec![("a.txt".to_string(), Status::Modified)]
    );
    assert_eq!(r.summary.modified, 1);
}

#[test]
fn a_changed_directory_is_structure_not_a_finding() {
    // A directory's digest moves whenever anything under it does.
    let r = Case {
        modified: vec![(dir("src", ALPHA), dir("src", BETA))],
        ..Default::default()
    }
    .run();
    assert!(statuses(&r, Side::Left).is_empty());
    assert_eq!(r.node(Side::Left, "src").unwrap().status, Status::Structure);
    assert_eq!(r.summary.modified, 0);
}

#[test]
fn the_root_row_is_never_a_finding() {
    // fsindex records the scan root as a row with an empty path.
    let r = Case {
        modified: vec![(dir("", ALPHA), dir("", BETA))],
        ..Default::default()
    }
    .run();
    assert!(r.left.nodes.is_empty());
}

// ---------------------------------------------------------------------
// in-tree duplicates
// ---------------------------------------------------------------------

#[test]
fn repeated_bytes_are_grouped() {
    let groups = group_duplicates(&[
        sized_file("a/x.bin", ALPHA, 1000),
        sized_file("b/x.bin", ALPHA, 1000),
        sized_file("c/unique.bin", BETA, 1000),
    ]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].paths, vec!["a/x.bin", "b/x.bin"]);
    assert_eq!(groups[0].wasted(), 1000);
}

#[test]
fn a_lone_entry_is_not_a_duplicate() {
    assert!(group_duplicates(&[file("only.bin", ALPHA)]).is_empty());
}

#[test]
fn a_duplicated_directory_absorbs_its_children() {
    let groups = group_duplicates(&[
        sized_dir("themes/dark", ALPHA, 50),
        sized_dir("themes/dark_backup", ALPHA, 50),
        sized_file("themes/dark/base.css", BETA, 25),
        sized_file("themes/dark_backup/base.css", BETA, 25),
    ]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kind, "dir");
    assert_eq!(groups[0].paths, vec!["themes/dark", "themes/dark_backup"]);
    assert_eq!(groups[0].rolled_up, 1);
}

#[test]
fn three_copies_report_two_wasted() {
    let groups = group_duplicates(&[
        sized_file("c0/x.bin", ALPHA, 64),
        sized_file("c1/x.bin", ALPHA, 64),
        sized_file("c2/x.bin", ALPHA, 64),
    ]);
    assert_eq!(groups[0].wasted(), 128);
}

#[test]
fn duplicates_reach_the_result() {
    let r = Case {
        right_dups: vec![
            sized_file("a/x.bin", ALPHA, 64),
            sized_file("b/x.bin", ALPHA, 64),
        ],
        ..Default::default()
    }
    .run();
    let dup = r.node(Side::Right, "a/x.bin").unwrap().dup.clone().unwrap();
    assert_eq!(dup.n, 2);
    assert_eq!(dup.peers, vec!["b/x.bin"]);
    assert_eq!(r.right.dup_wasted, 64);
}

#[test]
fn a_duplicate_the_diff_never_mentioned_still_appears() {
    // Unchanged between the trees, but repeated inside one of them.
    let r = Case {
        right_dups: vec![
            sized_file("a/x.bin", ALPHA, 64),
            sized_file("b/x.bin", ALPHA, 64),
        ],
        ..Default::default()
    }
    .run();
    assert!(r.node(Side::Right, "a/x.bin").is_some());
}

// ---------------------------------------------------------------------
// tree assembly
// ---------------------------------------------------------------------

#[test]
fn ancestor_directories_are_synthesised() {
    let r = Case {
        added: vec![file("deep/nested/leaf.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.node(Side::Right, "deep").unwrap().status,
        Status::Structure
    );
    assert_eq!(
        r.node(Side::Right, "deep/nested").unwrap().status,
        Status::Structure
    );
    assert_eq!(
        r.node(Side::Right, "deep/nested/leaf.txt").unwrap().status,
        Status::Added
    );
}

#[test]
fn changes_only_omits_untouched_entries() {
    let r = Case {
        added: vec![file("new.txt", ALPHA)],
        ..Default::default()
    }
    .run();
    assert!(r.node(Side::Right, "untouched.txt").is_none());
    assert!(!r.summary.full_tree);
}

#[test]
fn full_tree_includes_untouched_entries_as_unchanged() {
    let r = Case {
        added: vec![file("new.txt", ALPHA)],
        left_full: Some(vec![]),
        right_full: Some(vec![file("new.txt", ALPHA), file("untouched.txt", BETA)]),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.node(Side::Right, "untouched.txt").unwrap().status,
        Status::Unchanged
    );
    assert!(r.summary.full_tree);
}

#[test]
fn full_tree_does_not_relabel_a_rolled_up_move_as_a_separate_move() {
    // The interior of a moved directory is quiet, but it must not
    // claim to be its own finding either.
    let r = Case {
        removed: vec![dir("docs", ALPHA), file("docs/q3.txt", BETA)],
        added: vec![dir("archive", ALPHA), file("archive/q3.txt", BETA)],
        left_full: Some(vec![]),
        right_full: Some(vec![dir("archive", ALPHA), file("archive/q3.txt", BETA)]),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.node(Side::Right, "archive/q3.txt").unwrap().status,
        Status::Unchanged
    );
    assert_eq!(
        r.node(Side::Right, "archive").unwrap().status,
        Status::MovedIn
    );
}

// ---------------------------------------------------------------------
// the JSON contract
// ---------------------------------------------------------------------

fn rich() -> DiffResult {
    Case {
        removed: vec![
            dir("docs", ALPHA),
            file("docs/q3.txt", BETA),
            file("dropped.txt", GAMMA),
            file("dup/copy.txt", DELTA),
        ],
        added: vec![
            dir("archive", ALPHA),
            file("archive/q3.txt", BETA),
            file("brand_new.txt", "EEEE"),
        ],
        modified: vec![(file("a.txt", ALPHA), file("a.txt", BETA))],
        copies_right: [(DELTA.to_string(), "kept/original.txt".to_string())].into(),
        right_dups: vec![
            sized_file("x/big.bin", "FFFF", 4096),
            sized_file("y/big.bin", "FFFF", 4096),
        ],
        ..Default::default()
    }
    .run()
}

#[test]
fn the_result_round_trips_through_json() {
    // Serde makes this the representation rather than a debug dump: a
    // run captured with `--json` reads straight back.
    let original = rich();
    let text = serde_json::to_string(&original).unwrap();
    let restored: DiffResult = serde_json::from_str(&text).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn round_tripping_preserves_the_duplicate_groups() {
    // The count alone would be lossy — the groups carry paths.
    let original = rich();
    let restored: DiffResult =
        serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    assert_eq!(restored.right.dup_groups.len(), 1);
    assert_eq!(
        restored.right.dup_groups[0].paths,
        vec!["x/big.bin", "y/big.bin"]
    );
    assert_eq!(restored.right.dup_wasted, original.right.dup_wasted);
}

#[test]
fn round_tripping_preserves_peers_notes_and_rollups() {
    let restored: DiffResult =
        serde_json::from_str(&serde_json::to_string(&rich()).unwrap()).unwrap();
    let moved = restored.node(Side::Left, "docs").unwrap();
    assert_eq!(moved.status, Status::MovedOut);
    assert_eq!(moved.peer.as_deref(), Some("archive"));
    assert_eq!(moved.rolled_up, 1);
    let kept = restored.node(Side::Left, "dup/copy.txt").unwrap();
    assert_eq!(kept.status, Status::RemovedButCopyRemains);
    assert!(kept.note.contains("kept/original.txt"));
}

#[test]
fn the_status_strings_are_what_the_viewer_switches_on() {
    // `viewer.html.tmpl`'s BADGE table keys on these exact strings.
    let json = serde_json::to_string(&rich()).unwrap();
    for expected in [
        "\"moved_out\"",
        "\"moved_in\"",
        "\"removed\"",
        "\"removed_but_copy_remains\"",
        "\"added\"",
        "\"modified\"",
    ] {
        assert!(json.contains(expected), "missing {expected}");
    }
}

// ---------------------------------------------------------------------
// diff weight, for sorting big changes to the top
// ---------------------------------------------------------------------

fn weight(r: &DiffResult, side: Side, path: &str) -> (u32, i64) {
    let n = r.node(side, path).unwrap();
    (n.diff_entries, n.diff_bytes)
}

#[test]
fn a_directory_carries_the_weight_of_the_changes_beneath_it() {
    let r = Case {
        added: vec![
            sized_file("busy/a.txt", ALPHA, 100),
            sized_file("busy/b.txt", BETA, 200),
            sized_file("quiet/c.txt", GAMMA, 5),
        ],
        ..Default::default()
    }
    .run();
    assert_eq!(weight(&r, Side::Right, "busy"), (2, 300));
    assert_eq!(weight(&r, Side::Right, "quiet"), (1, 5));
    assert_eq!(weight(&r, Side::Right, "busy/a.txt"), (1, 100));
}

#[test]
fn weight_accumulates_through_intermediate_directories() {
    let r = Case {
        added: vec![sized_file("a/b/c/deep.txt", ALPHA, 42)],
        ..Default::default()
    }
    .run();
    for dir in ["a", "a/b", "a/b/c"] {
        assert_eq!(
            weight(&r, Side::Right, dir),
            (1, 42),
            "{dir} did not inherit its descendant's weight"
        );
    }
}

/// A directory's `size` is the recursive sum of its contents, so a
/// brand-new tree must not have those bytes counted twice — once for
/// the directory and again for each file in it.
#[test]
fn a_new_directory_does_not_double_count_its_new_files() {
    let r = Case {
        added: vec![
            sized_dir("fresh", ALPHA, 300),
            sized_file("fresh/a.txt", BETA, 100),
            sized_file("fresh/b.txt", GAMMA, 200),
        ],
        ..Default::default()
    }
    .run();
    let (entries, bytes) = weight(&r, Side::Right, "fresh");
    assert_eq!(
        bytes, 300,
        "the directory's own 300 bytes were counted on top of its files'"
    );
    assert_eq!(
        entries, 3,
        "the directory and both files are each an affected entry"
    );
}

/// The mirror case: a rolled-up move has no findings beneath it — its
/// interior is absent from the node set — so the directory itself must
/// supply the weight, or a 10 GB move would read as zero.
#[test]
fn a_rolled_up_move_carries_its_whole_subtree_s_weight() {
    let r = Case {
        removed: vec![
            sized_dir("docs", ALPHA, 5000),
            sized_dir("docs/reports", BETA, 5000),
            sized_file("docs/reports/q3.txt", GAMMA, 5000),
        ],
        added: vec![
            sized_dir("archive", ALPHA, 5000),
            sized_dir("archive/reports", BETA, 5000),
            sized_file("archive/reports/q3.txt", GAMMA, 5000),
        ],
        ..Default::default()
    }
    .run();
    let (entries, bytes) = weight(&r, Side::Right, "archive");
    assert_eq!(bytes, 5000, "the moved subtree's bytes went missing");
    assert_eq!(
        entries, 3,
        "one finding plus the two entries its rollup absorbed"
    );
}

#[test]
fn untouched_entries_weigh_nothing() {
    let r = Case {
        added: vec![sized_file("new.txt", ALPHA, 10)],
        right_full: Some(vec![
            sized_file("new.txt", ALPHA, 10),
            sized_file("untouched.txt", BETA, 9999),
        ]),
        left_full: Some(vec![]),
        ..Default::default()
    }
    .run();
    assert_eq!(weight(&r, Side::Right, "untouched.txt"), (0, 0));
    assert_eq!(weight(&r, Side::Right, "new.txt"), (1, 10));
}

/// The ordering the viewer defaults to has to be derivable from the
/// result alone, so it is asserted here rather than in the page.
#[test]
fn the_busiest_directory_outweighs_the_others() {
    let r = Case {
        added: vec![
            sized_file("small/a.txt", ALPHA, 1),
            sized_file("huge/b.bin", BETA, 1_000_000),
            sized_file("medium/c.txt", GAMMA, 500),
        ],
        ..Default::default()
    }
    .run();
    let mut dirs: Vec<(&str, i64)> = ["small", "huge", "medium"]
        .iter()
        .map(|d| (*d, r.node(Side::Right, d).unwrap().diff_bytes))
        .collect();
    dirs.sort_by_key(|(_, b)| -b);
    assert_eq!(
        dirs.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
        vec!["huge", "medium", "small"]
    );
}
