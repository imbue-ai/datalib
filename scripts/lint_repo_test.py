"""lint_repo.py's checks on what can be misspelled: checks 11 and 20."""

from __future__ import annotations

import unittest

from lint_repo import (
    checkout_token_settings,
    floating_npx,
    release_grant_problems,
    unliteral_opens,
)


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


class WorkflowTokenTest(unittest.TestCase):
    def test_a_checkout_says_what_it_does_with_the_token(self) -> None:
        text = """
steps:
  - uses: actions/checkout@abc # bare
  - run: echo persist-credentials: false
  - uses: actions/checkout@abc
    with:
      fetch-depth: 0
      persist-credentials: false
  - uses: actions/checkout@abc
    with:
      persist-credentials: true
"""
        self.assertEqual(
            [setting for _, setting in checkout_token_settings(text)],
            [None, "false", "true"],
        )

    GOOD = """
permissions:
  contents: read

jobs:
  runtime:
    runs-on: x
    steps:
      - run: bazel build //...
  publish-runtime:
    permissions:
      contents: write  # upload
  publish-tarballs:
    permissions:
      contents: write
  publish-macos-app:
    permissions:
      contents: write
  macos-app-sign:
    permissions:
      contents: read
      id-token: write
    steps:
      # no bazel here
      - run: ./datalib/tauri/build-signed-app.sh sign
  docker-publish:
    permissions:
      contents: read
      packages: write
"""

    def test_the_table_passes(self) -> None:
        self.assertEqual(release_grant_problems(self.GOOD), [])

    def test_a_write_default_is_refused(self) -> None:
        """The shape #1602 and earlier shipped: every build job inherited it."""
        text = self.GOOD.replace("contents: read\n\njobs", "contents: write\n\njobs")
        self.assertEqual(
            release_grant_problems(text),
            ["the workflow's default grants contents: write"],
        )

    def test_write_on_a_build_job_is_refused(self) -> None:
        text = self.GOOD.replace(
            "    runs-on: x\n",
            "    permissions:\n      contents: write\n    runs-on: x\n",
        )
        self.assertEqual(
            release_grant_problems(text), ["job runtime grants contents: write"]
        )

    def test_the_signing_grant_on_the_compile_job_is_refused(self) -> None:
        """id-token reaches every step's environment, so a build script could mint the Vault token."""
        text = self.GOOD.replace(
            "  runtime:\n",
            "  macos-app-compile:\n    permissions:\n      id-token: write\n",
        )
        self.assertEqual(
            release_grant_problems(text),
            ["job macos-app-compile grants id-token: write"],
        )

    def test_a_build_in_a_job_holding_a_grant_is_refused(self) -> None:
        for command in [
            "bazel build //x",
            "cargo build --release",
            "pnpm exec tauri build",
            "./b/build-signed-app.sh compile",
        ]:
            with self.subTest(command=command):
                text = self.GOOD.replace("build-signed-app.sh sign", command)
                self.assertEqual(len(release_grant_problems(text)), 1)
                self.assertIn("runs a build", release_grant_problems(text)[0])

    def test_a_publish_job_that_lost_its_grant_is_refused(self) -> None:
        """A tag is the first thing that would run it, and it would cost the release."""
        text = self.GOOD.replace(
            "  publish-tarballs:\n    permissions:\n      contents: write\n",
            "  publish-tarballs:\n    runs-on: x\n",
        )
        self.assertEqual(
            release_grant_problems(text),
            ["job publish-tarballs needs contents: write to do its work"],
        )

    def test_only_the_read_only_cache_key_may_appear(self) -> None:
        """A cold release could tempt someone to paste a write key back; a build script could use it."""
        with_key = self.GOOD.replace(
            "      - run: bazel build //...\n",
            "      - run: bazel build //...\n"
            "        env:\n"
            "          K: ${{ secrets.BUILD_BUDDY_RELEASE_READONLY_API_KEY }}\n",
        )
        self.assertEqual(release_grant_problems(with_key), [])
        for name in ["BUILD_BUDDY_RELEASE_API_KEY", "BUILD_BUDDY_API_KEY"]:
            with self.subTest(name=name):
                text = with_key.replace("BUILD_BUDDY_RELEASE_READONLY_API_KEY", name)
                self.assertEqual(
                    release_grant_problems(text),
                    [f"names {name}: a build job may hold only the read-only key"],
                )


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
