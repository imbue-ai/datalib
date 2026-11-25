"""`sql_scans_test.check` refuses a scan the allowlist does not name, and an
entry naming a statement that no longer scans, so the list can only shrink
as statements are fixed.
"""

import unittest

from sql_scans_test import check

SCAN = "SELECT id FROM t WHERE v = ?"
GONE = "SELECT id FROM t WHERE w = ?"


def entry(statement: str) -> dict[str, str]:
    return {"statement": statement, "why": "a reason"}


def row(
    statement: str, scans: bool = True, judged_by: str = "plan"
) -> dict[str, object]:
    return {
        "statement": statement,
        "files": ["*/s.db"],
        "judged_by": [judged_by],
        "scans": scans,
    }


class Check(unittest.TestCase):
    def test_a_listed_scan_passes(self):
        self.assertEqual(check([row(SCAN)], {"scan": [entry(SCAN)]}), [])

    def test_an_unlisted_scan_fails_with_a_stanza_to_paste(self):
        [error] = check([row(SCAN)], {"scan": []})
        self.assertIn(f'statement = "{SCAN}"', error)

    def test_an_entry_the_sync_no_longer_runs_fails(self):
        [error] = check([row(SCAN)], {"scan": [entry(SCAN), entry(GONE)]})
        self.assertIn("no longer runs", error)
        self.assertIn(GONE, error)

    def test_an_entry_whose_plan_no_longer_scans_fails(self):
        recorded = [row(SCAN), row(GONE, scans=False)]
        [error] = check(recorded, {"scan": [entry(SCAN), entry(GONE)]})
        self.assertIn("no longer walks", error)

    def test_an_entry_judged_by_the_rows_it_walked_may_walk_none(self):
        recorded = [row(SCAN), row(GONE, scans=False, judged_by="ran")]
        self.assertEqual(check(recorded, {"scan": [entry(SCAN), entry(GONE)]}), [])

    def test_an_entry_with_no_reason_fails(self):
        [error] = check([row(SCAN)], {"scan": [{"statement": SCAN, "why": " "}]})
        self.assertIn("no reason", error)

    def test_a_read_of_the_catalog_alone_needs_no_entry(self):
        """Doltlite's own read names its schema in quotes; the rule missed it."""
        for statement in [
            "SELECT name FROM sqlite_master WHERE type = 'table'",
            'SELECT name, sql FROM "main".sqlite_master WHERE rootpage>0',
            "SELECT name FROM 'main'.'sqlite_schema'",
        ]:
            self.assertEqual(check([row(statement)], {"scan": []}), [], statement)
        [error] = check([row('SELECT x FROM "main".t')], {"scan": []})
        self.assertIn("walks a whole table", error)

    def test_two_entries_for_one_statement_fail(self):
        """The run store's newest-run read was listed twice, with two
        reasons, and nothing noticed."""
        [error] = check([row(SCAN)], {"scan": [entry(SCAN), entry(SCAN)]})
        self.assertIn("two entries", error)


if __name__ == "__main__":
    unittest.main()
