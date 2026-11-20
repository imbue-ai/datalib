"""The checks `search_eval.py` trusts: an order check that cannot fail would
report a wrong order as a pass, a left-out source that hid a target would
report a miss the search never made, and a statement filed under the wrong
query would send someone to read the wrong plan."""

import datetime
import json
import unittest

from search_eval import (
    attribute,
    left_out,
    out_of_order,
    statement_from_log_line,
    statement_from_run_store,
    statements_table,
)


def rows(field, *values):
    return [{field: v} for v in values]


class OutOfOrder(unittest.TestCase):
    def test_sizes_in_order_pass_and_out_of_order_fail(self):
        self.assertIsNone(out_of_order(rows("byte_size", 30, 20, 10), "byte_size:desc"))
        self.assertIsNone(out_of_order(rows("byte_size", 10, 20, 30), "byte_size:asc"))
        self.assertIsNotNone(
            out_of_order(rows("byte_size", 10, 30, 20), "byte_size:desc")
        )
        self.assertIsNotNone(
            out_of_order(rows("byte_size", 10, 30, 20), "byte_size:asc")
        )

    def test_a_bare_column_runs_ascending_as_the_server_reads_it(self):
        self.assertIsNone(out_of_order(rows("byte_size", 10, 20), "byte_size"))
        self.assertIsNotNone(out_of_order(rows("byte_size", 20, 10), "byte_size"))

    def test_a_blank_comes_last_whichever_way_the_column_runs(self):
        self.assertIsNone(
            out_of_order(rows("byte_size", 10, 20, None), "byte_size:asc")
        )
        self.assertIsNone(
            out_of_order(rows("byte_size", 20, 10, None), "byte_size:desc")
        )
        self.assertIsNotNone(out_of_order(rows("byte_size", None, 10), "byte_size:asc"))
        self.assertIsNotNone(out_of_order(rows("author", "", "Data"), "author_ref:asc"))

    def test_text_ignores_case(self):
        names = rows("author", "Admiral", "jlpicard", "Me")
        self.assertIsNone(out_of_order(names, "author_ref:asc"))
        self.assertIsNotNone(
            out_of_order(rows("author", "Me", "jlpicard"), "author_ref:asc")
        )

    def test_stamps_compare_as_instants_whatever_their_offsets(self):
        # 09:00+02:00 is before 08:00Z, though it reads later.
        stamps = rows("touched_at", "2026-01-01T09:00:00+02:00", "2026-01-01T08:00:00Z")
        self.assertIsNone(out_of_order(stamps, "touched_at:asc"))
        self.assertIsNotNone(out_of_order(stamps, "touched_at:desc"))

    def test_only_the_first_column_is_checked_and_an_unknown_one_not_at_all(self):
        assert (
            out_of_order(rows("author", "A", "B"), "author_ref:asc,touched_at:desc")
            is None
        )
        self.assertIsNone(out_of_order(rows("channel", "b", "a"), "channel:asc"))


class LeftOut(unittest.TestCase):
    def test_a_query_leaves_out_every_source_but_its_targets_own(self):
        target = {"source_id": "gmail"}
        self.assertEqual(
            left_out("lunch friday", target, ["claude-code", "codex"]),
            "lunch friday -source:claude-code -source:codex",
        )
        self.assertEqual(
            left_out("lunch friday", {"source_id": "codex"}, ["claude-code", "codex"]),
            "lunch friday -source:claude-code",
        )

    def test_a_query_that_names_a_source_is_asked_as_written(self):
        q = "lunch source:gmail"
        self.assertEqual(left_out(q, {"source_id": "gmail"}, ["claude-code"]), q)
        q = "lunch -source:slack"
        self.assertEqual(left_out(q, {"source_id": "gmail"}, ["claude-code"]), q)

    def test_nothing_left_out_asks_the_query_unchanged(self):
        self.assertEqual(left_out("lunch", {"source_id": "gmail"}, []), "lunch")


if __name__ == "__main__":
    unittest.main()


# One instant, which both lines below spell: one in +01:00, one in UTC.
NINE_UTC = datetime.datetime(2026, 11, 20, 9, 0, 1, 500000, datetime.UTC).timestamp()

HEAVY = {
    "message": "heavy statement: 5800000 steps for 0 rows",
    "statement": "SELECT n.uuid FROM data_nodes n WHERE n.source_id = ?",
    "file": "/root/unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite",
    "params": "text(12)",
    "vm_steps": 5800000,
    "fullscan_steps": 0,
    "rows": 0,
    "elapsed_secs": 3.2,
}


class Statements(unittest.TestCase):
    def test_the_applets_own_line_is_read_and_others_are_not(self):
        line = json.dumps(
            {
                "timestamp": "2026-11-20T10:00:01.500000+01:00",
                "level": "WARN",
                "fields": HEAVY,
                "target": "datalib::sql_work",
            }
        )
        found = statement_from_log_line(line)
        assert found is not None
        self.assertEqual(found["kind"], "heavy")
        self.assertEqual(found["vm_steps"], 5800000)
        self.assertEqual(found["at"], NINE_UTC)
        info = json.loads(line) | {"level": "INFO"}
        self.assertIsNone(statement_from_log_line(json.dumps(info)))
        other = json.loads(line) | {"target": "datalib_applet"}
        self.assertIsNone(statement_from_log_line(json.dumps(other)))
        self.assertIsNone(statement_from_log_line("not json"))

    def test_a_slow_statement_from_the_servers_log_is_read(self):
        inner = {"db.statement": "\n\nSELECT hash FROM vectors\n", "elapsed_secs": 1.3}
        fields = json.dumps(
            {
                "applet": "unified_grid_and_search_index",
                "applet_target": "sqlx::query",
                "applet_fields": json.dumps(inner),
            }
        )
        found = statement_from_run_store("2026-11-20T09:00:01.500000+00:00", fields)
        assert found is not None
        self.assertEqual(found["kind"], "slow")
        self.assertEqual(found["statement"], "SELECT hash FROM vectors")
        self.assertEqual(found["at"], NINE_UTC)

    def test_each_line_goes_under_the_query_asked_when_it_was_written(self):
        logged = [{"at": t, "kind": "slow", "statement": str(t)} for t in (1, 5, 7, 12)]
        by_query, outside = attribute(logged, [(4, 6), (6.5, 10)])
        self.assertEqual([[s["at"] for s in q] for q in by_query], [[5], [7]])
        self.assertEqual([s["at"] for s in outside], [1, 12])

    def test_the_report_names_the_query_whose_statements_did_most(self):
        slow = {"kind": "slow", "statement": "S", "elapsed_secs": 1.1}
        light = {"id": "A", "q": "light", "statements": [slow]}
        heavy = {"id": "B", "q": "a | b", "statements": [{"kind": "heavy", **HEAVY}]}
        none = {"id": "C", "q": "none", "statements": []}
        lines = statements_table([light, heavy, none])
        rows = [line for line in lines if line.startswith(("| A", "| B", "| C"))]
        self.assertEqual(len(rows), 2, lines)
        self.assertTrue(
            rows[0].startswith(
                "| B | a \\| b | 1 | 0 | 5.8M steps for 0 rows | db.sqlite |"
            ),
            rows[0],
        )
        self.assertEqual(statements_table([none]), [])
