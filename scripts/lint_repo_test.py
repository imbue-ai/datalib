"""lint_repo.py's checks on what can be misspelled: checks 11 and 20."""

from __future__ import annotations

import unittest

from lint_repo import floating_npx, unliteral_opens


class FloatingNpxTest(unittest.TestCase):
    def test_every_runner_and_spelling_without_an_exact_version_is_refused(
        self,
    ) -> None:
        """Each of these got past the check's first pattern."""
        for line in [
            "npx --yes latchkey auth set slack",
            "npx -y latchkey@latest auth browser slack",
            "npx -y latchkey@3 auth list",
            "pnpm dlx latchkey auth list",
            "bunx latchkey auth list",
            "bun x latchkey auth list",
            "npm exec latchkey -- auth list",
            "npm exec -- latchkey auth list",
            "yarn dlx @tobilu/qmd status",
            "npx -y -p latchkey latchkey auth list",
            "npx --package=latchkey@next latchkey curl https://slack.test",
            "npx -y latchkey",
            "run: npx -y @tobilu/qmd",
        ]:
            with self.subTest(line=line):
                self.assertTrue(floating_npx(line))

    def test_an_exact_version_or_the_pin_as_a_placeholder_passes(self) -> None:
        for line in [
            "npx -y latchkey@3.21.0 auth list",
            "npx -y latchkey@<LATCHKEY_VERSION> auth clear -y",
            'cmd = f"npx -y latchkey@{LATCHKEY_VERSION} auth list"',
            'npx -y "latchkey@${LATCHKEY_VERSION}" auth list',
            "npx -y -p latchkey@3.21.0 latchkey auth list",
            "npx -y @tobilu/qmd@2.8.3 status",
            "latchkey auth browser slack",
            "npx -y vite build",
            "pnpm exec vue-tsc",
        ]:
            with self.subTest(line=line):
                self.assertFalse(floating_npx(line))

    def test_a_mention_in_a_backtick_span_is_prose(self) -> None:
        self.assertFalse(
            floating_npx("Docs said to run bare `npx -y latchkey` (no version).")
        )
        self.assertTrue(floating_npx("Run `npx -y latchkey auth list` to see them."))


class LiteralOpenTest(unittest.TestCase):
    def test_a_url_or_a_bare_filename_is_refused(self) -> None:
        for source in [
            'SqliteConnectOptions::from_str(&format!("sqlite://{}", p.display()))',
            'pool.connect(&format!("sqlite://{}?mode=ro", p.display()))',
            'from_str(&format!(\n    "sqlite://{}",\n    p.display()))',
            'pool.connect(&format!("sqlite:{}", p.display()))',
            "SqliteConnectOptions::new().filename(path)",
            "SqliteConnectOptions::new()\n    .filename(&db)",
        ]:
            with self.subTest(source=source):
                self.assertTrue(unliteral_opens(source))

    def test_the_helper_the_plain_uri_and_memory_pass(self) -> None:
        for source in [
            "datalib_sqlite_open::file(path).read_only(true)",
            'datalib_sqlite_open::revision(path, "main")',
            'SqliteConnectOptions::from_str("sqlite::memory:")',
            ".filename(plain_sqlite::uri(path))",
            ".filename(datalib_runtime::plain_sqlite::uri(path))",
            ".filename(\n    crate::plain_sqlite::uri(path))",
        ]:
            with self.subTest(source=source):
                self.assertEqual(unliteral_opens(source), [])


if __name__ == "__main__":
    unittest.main()
