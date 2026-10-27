"""check_tzdata_current.py's parsing and release order, without the network."""

from __future__ import annotations

import io
import tarfile
import unittest

from check_tzdata_current import bundled_release, locked_crate_version, release_key


def crate_with(tzname_rs: str) -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        data = tzname_rs.encode()
        info = tarfile.TarInfo("jiff-tzdb-0.1.9/tzname.rs")
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


class TzdataTest(unittest.TestCase):
    def test_releases_order_by_year_then_letter(self) -> None:
        self.assertLess(release_key("2026c"), release_key("2026e"))
        self.assertLess(release_key("2026z"), release_key("2026za"))
        self.assertLess(release_key("2026za"), release_key("2027a"))
        with self.assertRaises(ValueError):
            release_key("v0.1.9")

    def test_the_locked_crate_and_its_release_are_read(self) -> None:
        lock = '[[package]]\nname = "jiff-tzdb"\nversion = "0.1.9"\nsource = "x"\n'
        self.assertEqual(locked_crate_version(lock), "0.1.9")
        tzname = 'pub(super) static VERSION: Option<&str> = Some(r"2026e");\n'
        self.assertEqual(bundled_release(crate_with(tzname)), "2026e")


if __name__ == "__main__":
    unittest.main()
