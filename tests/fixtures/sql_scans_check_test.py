"""`sql_scans_test.check` refuses a scan the allowlist does not name, and an
entry naming a statement that no longer scans, so the list can only shrink
as statements are fixed.
"""

import unittest

from sql_scans_test import check

SCAN = "SELECT id FROM t WHERE v = ?"
GONE = "SELECT id FROM t WHERE w = ?"


def entry(statement: str, **extra: str) -> dict[str, str]:
    return {"statement": statement, "why": "a reason", **extra}


def row(statement: str, judged_by: str = "plan") -> dict[str, object]:
    return {"statement": statement, "files": ["*/s.db"], "judged_by": [judged_by]}


class Check(unittest.TestCase):
    def test_a_listed_scan_passes(self):
        self.assertEqual(check([row(SCAN)], {"scan": [entry(SCAN)]}), [])

    def test_an_unlisted_scan_fails_with_a_stanza_to_paste(self):
        [error] = check([row(SCAN)], {"scan": []})
        self.assertIn(f"statement = \"{SCAN}\"", error)
        self.assertNotIn("judged_by", error)

    def test_the_stanza_for_a_scan_judged_by_what_it_did_says_so(self):
        [error] = check([row(SCAN, "ran")], {"scan": []})
        self.assertIn('judged_by = "ran"', error)

    def test_an_entry_whose_statement_no_longer_scans_fails(self):
        """A stale entry was only a note, so an entry outlived its scan and
        waved through any later statement of the same text."""
        [error] = check([row(SCAN)], {"scan": [entry(SCAN), entry(GONE)]})
        self.assertIn("delete its entry", error)
        self.assertIn(GONE, error)

    def test_an_entry_judged_by_what_it_did_may_be_missing_from_a_run(self):
        allowed = {"scan": [entry(SCAN), entry(GONE, judged_by="ran")]}
        self.assertEqual(check([row(SCAN)], allowed), [])

    def test_an_entry_with_no_reason_fails(self):
        [error] = check([row(SCAN)], {"scan": [{"statement": SCAN, "why": " "}]})
        self.assertIn("no reason", error)

    def test_two_entries_for_one_statement_fail(self):
        [error] = check([row(SCAN)], {"scan": [entry(SCAN), entry(SCAN)]})
        self.assertIn("two entries", error)


if __name__ == "__main__":
    unittest.main()
