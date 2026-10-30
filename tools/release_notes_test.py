"""release_notes.py's reading of ladders and shapes, without git."""

from __future__ import annotations

import unittest

from release_notes import render, rungs_in, store_changes, tables_in

LADDER_RS = """
pub const LADDER: &[Migration] = &[
    Migration {
        version: 1,
        name: "threads lose their account index",
        apply: |conn| Box::pin(async move { Ok(()) }),
    },
    Migration {
        version: 2,
        name: "a \\"quoted\\" id is hex",
        apply: |conn| Box::pin(migrate(conn)),
    },
];

#[cfg(test)]
mod tests {
    fn rung() -> Migration {
        Migration { version: 9, name: "only a test", apply: noop }
    }
}
"""


def shape(version: int, *sql: str) -> dict:
    return {"schema_version": version, "shared_schema_version": 1, "sql": list(sql)}


class ReleaseNotesTest(unittest.TestCase):
    def test_rungs_are_read_and_test_modules_are_not(self) -> None:
        self.assertEqual(
            rungs_in(LADDER_RS),
            {1: "threads lose their account index", 2: 'a "quoted" id is hex'},
        )

    def test_columns_are_read_past_nested_parentheses(self) -> None:
        table = (
            "CREATE TABLE emails( id TEXT PRIMARY KEY, n INTEGER DEFAULT (0), "
            "thread_id TEXT NOT NULL, PRIMARY KEY(id, n))"
        )
        sql = [table, "CREATE INDEX emails_by_thread ON emails(thread_id)"]
        self.assertEqual(
            tables_in(sql),
            {
                "emails": {
                    "id": "TEXT PRIMARY KEY",
                    "n": "INTEGER DEFAULT (0)",
                    "thread_id": "TEXT NOT NULL",
                }
            },
        )

    def test_a_rung_with_no_shape_change_is_still_listed(self) -> None:
        """The Gmail thread id went from decimal to hex in a TEXT column
        (#1424): the shape did not move, so only the rung can say so."""
        table = "CREATE TABLE emails( id TEXT PRIMARY KEY, thread_id TEXT NOT NULL)"
        changes = store_changes(
            {"email": shape(1, table)},
            {"email": shape(2, table)},
            lambda _: rungs_in(LADDER_RS),
        )
        self.assertEqual(len(changes), 1)
        self.assertEqual(changes[0].rungs, ['a "quoted" id is hex (rung 2)'])
        self.assertIn(
            'Migrated on first open: a "quoted" id is hex',
            render("v1", [], changes, []),
        )

    def test_columns_and_tables_that_move_are_named(self) -> None:
        old = shape(
            0,
            "CREATE TABLE t( id TEXT, gone TEXT, kind TEXT)",
            "CREATE TABLE old( id TEXT)",
        )
        new = shape(
            0,
            "CREATE TABLE t( id TEXT, kind INTEGER, label_ids TEXT NULL)",
            "CREATE TABLE fresh( id TEXT)",
        )
        [change] = store_changes({"x": old}, {"x": new}, lambda _: {})
        self.assertEqual(
            change.breaking,
            [
                "table `old` is gone",
                "`t.gone` is gone",
                "`t.kind` was `TEXT`, is `INTEGER`",
            ],
        )
        self.assertEqual(
            change.added, ["new table `fresh`", "new column `t.label_ids` `TEXT NULL`"]
        )

    def test_a_ladder_it_cannot_read_fails_the_run(self) -> None:
        with self.assertRaises(SystemExit):
            store_changes({"email": shape(1)}, {"email": shape(3)}, lambda _: {1: "a"})

    def test_an_unchanged_store_is_left_out(self) -> None:
        same = shape(2, "CREATE TABLE t( id TEXT)")
        self.assertEqual(store_changes({"x": same}, {"x": same}, lambda _: {}), [])
        self.assertIn("No raw store changed", render("v1", [], [], ["A PR (#1)"]))


if __name__ == "__main__":
    unittest.main()
