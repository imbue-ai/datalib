"""Tests for the diff interpretation, with no database and no HTML.

Everything here drives [`dirtree_diff.analyze`], the pure function
between "what we read" ([`Inputs`]) and "what we concluded"
([`DiffResult`]). A test constructs the rows a doltlite prolly diff
would have produced and asserts on the resulting statuses — so the
move pairing, the subtree rollup, the delete-vs-copy-remains
distinction and the in-tree duplicate grouping are all pinned without
a `.doltlite_db` or a browser anywhere in the picture.

Run with `bazel test //hack/dirtree_diff:dirtree_diff_test`.
"""

from __future__ import annotations

import json
import unittest

from dirtree_diff import (
    Diff,
    DiffResult,
    Entry,
    Inputs,
    SideInput,
    analyze,
    group_duplicates,
    parse_size,
)

# A digest is only ever compared for equality, so the tests use short
# readable stand-ins rather than real 64-char blake3 hex.
ALPHA = "AAAA"
BETA = "BBBB"
GAMMA = "CCCC"
DELTA = "DDDD"


def file_entry(path: str, digest: str, size: int = 10) -> Entry:
    return Entry(path=path, kind="file", size=size, digest=digest)


def dir_entry(path: str, digest: str, size: int = 100) -> Entry:
    return Entry(path=path, kind="dir", size=size, digest=digest)


def run(
    *,
    removed: list[Entry] | None = None,
    added: list[Entry] | None = None,
    modified: list[tuple[Entry, Entry]] | None = None,
    copies_right: dict[str, str] | None = None,
    copies_left: dict[str, str] | None = None,
    left_full: dict[str, Entry] | None = None,
    right_full: dict[str, Entry] | None = None,
    left_dups: list[Entry] | None = None,
    right_dups: list[Entry] | None = None,
) -> DiffResult:
    """Analyze one hand-built diff."""
    return analyze(
        Inputs(
            left=SideInput(
                db="/l.doltlite_db",
                ref="HEAD",
                commit="1" * 40,
                full=left_full,
                dup_candidates=left_dups or [],
            ),
            right=SideInput(
                db="/r.doltlite_db",
                ref="HEAD",
                commit="2" * 40,
                full=right_full,
                dup_candidates=right_dups or [],
            ),
            diff=Diff(
                removed=removed or [],
                added=added or [],
                modified=modified or [],
            ),
            copies_right=copies_right or {},
            copies_left=copies_left or {},
        )
    )


class MoveDetection(unittest.TestCase):
    def test_same_digest_at_a_new_path_is_a_move(self):
        result = run(
            removed=[file_entry("notes.txt", ALPHA)],
            added=[file_entry("archive/notes.txt", ALPHA)],
        )
        self.assertEqual(result.statuses("left"), {"notes.txt": "moved_out"})
        self.assertEqual(result.statuses("right"), {"archive/notes.txt": "moved_in"})
        left = result.node("left", "notes.txt")
        assert left is not None
        self.assertEqual(left.peer, "archive/notes.txt")

    def test_a_different_digest_at_a_new_path_is_not_a_move(self):
        result = run(
            removed=[file_entry("notes.txt", ALPHA)],
            added=[file_entry("archive/notes.txt", BETA)],
        )
        self.assertEqual(result.statuses("left"), {"notes.txt": "removed"})
        self.assertEqual(result.statuses("right"), {"archive/notes.txt": "added"})

    def test_kind_must_match_too(self):
        """A file and a directory sharing a digest are not a move."""
        result = run(
            removed=[file_entry("thing", ALPHA)],
            added=[dir_entry("thing_dir", ALPHA)],
        )
        self.assertEqual(result.statuses("left"), {"thing": "removed"})
        self.assertEqual(result.statuses("right"), {"thing_dir": "added"})

    def test_pairing_prefers_the_candidate_that_kept_its_basename(self):
        """Two identical files vanish and two appear; basename decides."""
        result = run(
            removed=[
                file_entry("a/report.txt", ALPHA),
                file_entry("a/memo.txt", ALPHA),
            ],
            added=[file_entry("b/memo.txt", ALPHA), file_entry("b/report.txt", ALPHA)],
        )
        report = result.node("left", "a/report.txt")
        memo = result.node("left", "a/memo.txt")
        assert report is not None and memo is not None
        self.assertEqual(report.peer, "b/report.txt")
        self.assertEqual(memo.peer, "b/memo.txt")


class SubtreeRollup(unittest.TestCase):
    """A moved directory should be one row, not one row per descendant."""

    def moved_tree(self) -> DiffResult:
        return run(
            removed=[
                dir_entry("docs", ALPHA),
                dir_entry("docs/reports", BETA),
                file_entry("docs/reports/q3.txt", GAMMA),
            ],
            added=[
                dir_entry("archive", ALPHA),
                dir_entry("archive/reports", BETA),
                file_entry("archive/reports/q3.txt", GAMMA),
            ],
        )

    def test_only_the_outermost_directory_is_reported(self):
        result = self.moved_tree()
        self.assertEqual(result.statuses("left"), {"docs": "moved_out"})
        self.assertEqual(result.statuses("right"), {"archive": "moved_in"})

    def test_the_survivor_counts_what_it_absorbed(self):
        node = self.moved_tree().node("left", "docs")
        assert node is not None
        self.assertEqual(node.rolled_up, 2)
        self.assertEqual(self.moved_tree().summary["moves"], 1)
        self.assertEqual(self.moved_tree().summary["rolled_up"], 2)

    def test_a_descendant_that_moved_somewhere_else_survives_the_rollup(self):
        """Only entries making the *same* journey get absorbed."""
        result = run(
            removed=[
                dir_entry("docs", ALPHA),
                file_entry("docs/stray.txt", DELTA),
            ],
            added=[
                dir_entry("archive", ALPHA),
                file_entry("elsewhere/stray.txt", DELTA),
            ],
        )
        self.assertEqual(
            result.statuses("left"),
            {"docs": "moved_out", "docs/stray.txt": "moved_out"},
        )
        node = result.node("left", "docs")
        assert node is not None
        self.assertEqual(node.rolled_up, 0)


class DeletesAndCopies(unittest.TestCase):
    def test_delete_with_no_surviving_copy(self):
        result = run(removed=[file_entry("gone.txt", ALPHA)])
        self.assertEqual(result.statuses("left"), {"gone.txt": "removed"})
        self.assertEqual(result.summary["removed"], 1)

    def test_delete_whose_bytes_survive_elsewhere_on_the_right(self):
        result = run(
            removed=[file_entry("dup/copy.txt", ALPHA)],
            copies_right={ALPHA: "keep/original.txt"},
        )
        self.assertEqual(
            result.statuses("left"), {"dup/copy.txt": "removed_but_copy_remains"}
        )
        node = result.node("left", "dup/copy.txt")
        assert node is not None
        self.assertEqual(node.peer, "keep/original.txt")
        self.assertEqual(result.summary["removed"], 0)
        self.assertEqual(result.summary["removed_but_copy_remains"], 1)

    def test_new_content_is_an_add(self):
        result = run(added=[file_entry("fresh.txt", ALPHA)])
        self.assertEqual(result.statuses("right"), {"fresh.txt": "added"})

    def test_new_path_holding_pre_existing_bytes_is_a_copy(self):
        result = run(
            added=[file_entry("backup.txt", ALPHA)],
            copies_left={ALPHA: "original.txt"},
        )
        self.assertEqual(result.statuses("right"), {"backup.txt": "added_from_copy"})
        self.assertEqual(result.summary["added"], 0)
        self.assertEqual(result.summary["added_from_copy"], 1)

    def test_a_copied_directory_rolls_up(self):
        result = run(
            added=[
                dir_entry("themes/dark_backup", ALPHA),
                file_entry("themes/dark_backup/base.css", BETA),
            ],
            copies_left={ALPHA: "themes/dark", BETA: "themes/dark/base.css"},
        )
        self.assertEqual(
            result.statuses("right"), {"themes/dark_backup": "added_from_copy"}
        )
        node = result.node("right", "themes/dark_backup")
        assert node is not None
        self.assertEqual(node.rolled_up, 1)

    def test_copy_detection_off_downgrades_to_plain_delete_and_add(self):
        """Without the lookups, nothing may be *upgraded* to a copy."""
        result = run(
            removed=[file_entry("dup/copy.txt", ALPHA)],
            added=[file_entry("backup.txt", BETA)],
        )
        self.assertEqual(result.statuses("left"), {"dup/copy.txt": "removed"})
        self.assertEqual(result.statuses("right"), {"backup.txt": "added"})


class Modifications(unittest.TestCase):
    def test_a_changed_file_is_a_finding_on_both_sides(self):
        result = run(modified=[(file_entry("a.txt", ALPHA), file_entry("a.txt", BETA))])
        self.assertEqual(result.statuses("left"), {"a.txt": "modified"})
        self.assertEqual(result.statuses("right"), {"a.txt": "modified"})
        self.assertEqual(result.summary["modified"], 1)

    def test_a_changed_directory_is_structure_not_a_finding(self):
        """A directory's digest moves whenever anything under it does."""
        result = run(modified=[(dir_entry("src", ALPHA), dir_entry("src", BETA))])
        self.assertEqual(result.statuses("left"), {})
        node = result.node("left", "src")
        assert node is not None
        self.assertEqual(node.status, "structure")
        self.assertEqual(result.summary["modified"], 0)

    def test_the_root_row_is_never_a_finding(self):
        """fsindex records the scan root as a row with an empty path."""
        result = run(modified=[(dir_entry("", ALPHA), dir_entry("", BETA))])
        self.assertEqual(result.left.nodes, [])


class InTreeDuplicates(unittest.TestCase):
    def test_repeated_bytes_are_grouped(self):
        groups = group_duplicates(
            [
                file_entry("a/x.bin", ALPHA, size=1000),
                file_entry("b/x.bin", ALPHA, size=1000),
                file_entry("c/unique.bin", BETA, size=1000),
            ]
        )
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0].paths, ["a/x.bin", "b/x.bin"])
        self.assertEqual(groups[0].wasted, 1000)

    def test_a_lone_entry_is_not_a_duplicate(self):
        self.assertEqual(group_duplicates([file_entry("only.bin", ALPHA)]), [])

    def test_a_duplicated_directory_absorbs_its_children(self):
        groups = group_duplicates(
            [
                dir_entry("themes/dark", ALPHA, size=50),
                dir_entry("themes/dark_backup", ALPHA, size=50),
                file_entry("themes/dark/base.css", BETA, size=25),
                file_entry("themes/dark_backup/base.css", BETA, size=25),
            ]
        )
        self.assertEqual([g.kind for g in groups], ["dir"])
        self.assertEqual(groups[0].paths, ["themes/dark", "themes/dark_backup"])
        self.assertEqual(groups[0].rolled_up, 1)

    def test_three_copies_report_two_wasted(self):
        groups = group_duplicates(
            [file_entry(f"c{i}/x.bin", ALPHA, size=64) for i in range(3)]
        )
        self.assertEqual(groups[0].wasted, 128)

    def test_duplicates_reach_the_result_on_both_panes(self):
        dups = [
            file_entry("a/x.bin", ALPHA, size=64),
            file_entry("b/x.bin", ALPHA, size=64),
        ]
        result = run(right_dups=dups)
        node = result.node("right", "a/x.bin")
        assert node is not None and node.dup is not None
        self.assertEqual(node.dup["n"], 2)
        self.assertEqual(node.dup["peers"], ["b/x.bin"])
        self.assertEqual(result.right.dup_wasted, 64)

    def test_a_duplicate_that_the_diff_never_mentioned_still_appears(self):
        """Unchanged between the trees, but repeated inside one of them."""
        result = run(
            right_dups=[
                file_entry("a/x.bin", ALPHA, size=64),
                file_entry("b/x.bin", ALPHA, size=64),
            ]
        )
        self.assertIsNotNone(result.node("right", "a/x.bin"))


class TreeAssembly(unittest.TestCase):
    def test_ancestor_directories_are_synthesised(self):
        result = run(added=[file_entry("deep/nested/leaf.txt", ALPHA)])
        paths = {n.path: n.status for n in result.right.nodes}
        self.assertEqual(paths["deep"], "structure")
        self.assertEqual(paths["deep/nested"], "structure")
        self.assertEqual(paths["deep/nested/leaf.txt"], "added")

    def test_changes_only_omits_untouched_entries(self):
        result = run(added=[file_entry("new.txt", ALPHA)])
        self.assertNotIn("untouched.txt", {n.path for n in result.right.nodes})
        self.assertFalse(result.summary["full_tree"])

    def test_full_tree_includes_untouched_entries_as_unchanged(self):
        result = run(
            added=[file_entry("new.txt", ALPHA)],
            right_full={
                "new.txt": file_entry("new.txt", ALPHA),
                "untouched.txt": file_entry("untouched.txt", BETA),
            },
            left_full={},
        )
        node = result.node("right", "untouched.txt")
        assert node is not None
        self.assertEqual(node.status, "unchanged")
        self.assertTrue(result.summary["full_tree"])

    def test_full_tree_does_not_relabel_a_rolled_up_move_as_unchanged(self):
        """The interior of a moved directory is quiet, but it is not a
        finding either — it must not claim to be a separate move."""
        result = run(
            removed=[dir_entry("docs", ALPHA), file_entry("docs/q3.txt", BETA)],
            added=[dir_entry("archive", ALPHA), file_entry("archive/q3.txt", BETA)],
            right_full={
                "archive": dir_entry("archive", ALPHA),
                "archive/q3.txt": file_entry("archive/q3.txt", BETA),
            },
            left_full={},
        )
        interior = result.node("right", "archive/q3.txt")
        assert interior is not None
        self.assertEqual(interior.status, "unchanged")
        outermost = result.node("right", "archive")
        assert outermost is not None
        self.assertEqual(outermost.status, "moved_in")


class PayloadContract(unittest.TestCase):
    def test_the_payload_is_json_serialisable(self):
        result = run(
            removed=[file_entry("a.txt", ALPHA)],
            added=[file_entry("b.txt", ALPHA)],
            right_dups=[
                file_entry("x.bin", BETA, size=9),
                file_entry("y.bin", BETA, size=9),
            ],
        )
        blob = json.dumps(result.to_payload())
        self.assertIn('"moved_out"', blob)

    def test_the_payload_carries_both_sides_and_a_summary(self):
        payload = run().to_payload()
        self.assertEqual(set(payload), {"left", "right", "summary"})
        left = payload["left"]
        assert isinstance(left, dict)
        self.assertEqual(
            set(left),
            {"db", "ref", "commit", "dup_groups", "dup_wasted", "nodes"},
        )

    def rich_result(self) -> DiffResult:
        """One result exercising every field the payload carries."""
        return run(
            removed=[
                dir_entry("docs", ALPHA),
                file_entry("docs/q3.txt", BETA),
                file_entry("dropped.txt", GAMMA),
                file_entry("dup/copy.txt", DELTA),
            ],
            added=[
                dir_entry("archive", ALPHA),
                file_entry("archive/q3.txt", BETA),
                file_entry("brand_new.txt", "EEEE"),
            ],
            modified=[(file_entry("a.txt", ALPHA), file_entry("a.txt", BETA))],
            copies_right={DELTA: "kept/original.txt"},
            right_dups=[
                file_entry("x/big.bin", "FFFF", size=4096),
                file_entry("y/big.bin", "FFFF", size=4096),
            ],
        )

    def test_the_payload_round_trips(self):
        """`from_payload(to_payload(x))` must reproduce `x`.

        This is what makes the JSON the representation rather than a
        debug dump: a run captured with `--json` can be replayed into a
        test or another viewer with nothing lost.
        """
        original = self.rich_result()
        restored = DiffResult.from_payload(
            json.loads(json.dumps(original.to_payload()))
        )
        self.assertEqual(restored.left, original.left)
        self.assertEqual(restored.right, original.right)
        self.assertEqual(restored.summary, original.summary)
        self.assertEqual(restored, original)

    def test_round_trip_preserves_the_duplicate_groups(self):
        """The count alone would be lossy — the groups carry paths."""
        original = self.rich_result()
        restored = DiffResult.from_payload(original.to_payload())
        self.assertEqual(len(restored.right.dup_groups), 1)
        self.assertEqual(restored.right.dup_groups[0].paths, ["x/big.bin", "y/big.bin"])
        self.assertEqual(restored.right.dup_wasted, original.right.dup_wasted)

    def test_round_trip_preserves_peers_notes_and_rollups(self):
        restored = DiffResult.from_payload(self.rich_result().to_payload())
        moved = restored.node("left", "docs")
        assert moved is not None
        self.assertEqual(moved.status, "moved_out")
        self.assertEqual(moved.peer, "archive")
        self.assertEqual(moved.rolled_up, 1)
        kept = restored.node("left", "dup/copy.txt")
        assert kept is not None
        self.assertEqual(kept.status, "removed_but_copy_remains")
        self.assertEqual(kept.peer, "kept/original.txt")
        self.assertIn("kept/original.txt", kept.note)


class SizeParsing(unittest.TestCase):
    def test_plain_and_suffixed_sizes(self):
        self.assertEqual(parse_size("4096"), 4096)
        self.assertEqual(parse_size("64K"), 65536)
        self.assertEqual(parse_size("1M"), 1048576)
        self.assertEqual(parse_size("2G"), 2 * 1024**3)
        self.assertEqual(parse_size("1.5M"), 1572864)
        self.assertEqual(parse_size("0"), 0)

    def test_case_and_trailing_b_are_tolerated(self):
        self.assertEqual(parse_size("64k"), 65536)
        self.assertEqual(parse_size("64KB"), 65536)

    def test_nonsense_is_rejected(self):
        import argparse

        for bad in ("", "  ", "many", "12X"):
            with self.assertRaises(argparse.ArgumentTypeError):
                parse_size(bad)


if __name__ == "__main__":
    unittest.main()
