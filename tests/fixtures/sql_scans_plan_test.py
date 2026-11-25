"""`sql_scans.py` judges a statement by its plan, read through the doltlite
shell: a scan of an empty table is a scan, which counting the rows it walked
would miss, and a statement whose plan cannot be read falls back to what it
did as it ran.

Usage: sql_scans_plan_test.py <doltlite shell>
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from sql_scans import judge, plans_in

DOLTLITE = Path(sys.argv.pop(1))

LOOKUP = "SELECT v FROM t WHERE k = ?"
SCAN = "SELECT k FROM t WHERE v = ?"
QUOTED = "SELECT k FROM t WHERE v = 'it''s'"
UNPLANNABLE = "SELECT x FROM temp_only"


class Plans(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.store = Path(self.dir.name) / "store.doltlite_db"
        subprocess.run(
            [str(DOLTLITE), str(self.store)],
            input="CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT);\n",
            text=True,
            check=True,
        )

    def tearDown(self):
        self.dir.cleanup()

    def test_a_scan_of_an_empty_table_is_read_from_its_plan(self):
        plans = plans_in(DOLTLITE, self.store, [LOOKUP, SCAN, QUOTED, UNPLANNABLE])
        self.assertEqual(plans, {0: False, 1: True, 2: True})

    def test_a_statement_with_no_plan_is_judged_by_what_it_did(self):
        file = str(self.store)
        ran = {(SCAN, file): False, (UNPLANNABLE, file): True}
        plans = {(SCAN, file): True}
        self.assertEqual(
            judge(ran, plans),
            {(SCAN, file): (True, "plan"), (UNPLANNABLE, file): (True, "ran")},
        )


if __name__ == "__main__":
    unittest.main()
