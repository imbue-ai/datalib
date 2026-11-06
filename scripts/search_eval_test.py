"""The order check `search_eval.py first-pages` trusts: a check that cannot fail
would report a wrong order as a pass."""

import unittest

from search_eval import out_of_order


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


if __name__ == "__main__":
    unittest.main()
