"""lint_repo.py's checks on what can be misspelled: checks 11 and 20."""

from __future__ import annotations

import unittest

from lint_repo import (
    checkout_token_settings,
    floating_images,
    floating_npx,
    reader_grant_problems,
    release_grant_problems,
    release_key_problems,
    unliteral_opens,
)


class ReaderWheelGrantsTest(unittest.TestCase):
    """The workflow that publishes to PyPI answers to the release's rules."""

    GOOD = """\
jobs:
  wheels:
    steps:
      - uses: bazel-contrib/setup-bazel@abc # 0.19.1
        with:
          bazelisk-cache: false
          disk-cache: false
          repository-cache: false
          external-cache: false
      - run: scripts/release/stage_reader_wheels.sh out
  publish:
    permissions:
      id-token: write
    steps:
      - uses: actions/download-artifact@abc # v8
        with:
          path: ${{ runner.temp }}/wheels
      - uses: pypa/gh-action-pypi-publish@abc # v1
"""

    def test_the_real_shape_passes(self) -> None:
        self.assertEqual(reader_grant_problems(self.GOOD), [])

    def test_a_build_cannot_hold_the_publish_grant(self) -> None:
        text = self.GOOD.replace(
            "  wheels:\n", "  wheels:\n    permissions:\n      id-token: write\n"
        )
        self.assertEqual(
            reader_grant_problems(text), ["job wheels grants id-token: write"]
        )

    def test_the_publish_job_cannot_build(self) -> None:
        text = self.GOOD.replace(
            "      - uses: pypa/", "      - run: bazel build //x\n      - uses: pypa/"
        )
        problems = reader_grant_problems(text)
        self.assertEqual(len(problems), 1)
        self.assertIn("runs a build", problems[0])

    def test_it_reads_no_cache(self) -> None:
        text = self.GOOD.replace("disk-cache: false", "disk-cache: true")
        self.assertEqual(
            reader_grant_problems(text),
            ["a release reads no cache: setup-bazel needs disk-cache: false"],
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
  publish-release:
    permissions:
      contents: write  # upload
  macos-app-sign:
    permissions:
      contents: read
      id-token: write
    steps:
      # no bazel here
      - uses: actions/download-artifact@abc # v8
        with:
          name: macos-app-compiled
          path: ${{ runner.temp }}/macos-app-compiled
      - run: python3 -I scripts/release/unpack_compiled_app.py x datalib/tauri
      - run: ./datalib/tauri/build-signed-app.sh sign
  docker-image:
    permissions:
      contents: read
      packages: write
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
            "  publish-release:\n    permissions:\n      contents: write  # upload\n",
            "  publish-release:\n    runs-on: x\n",
        )
        self.assertEqual(
            release_grant_problems(text),
            ["job publish-release needs contents: write to do its work"],
        )

    def test_a_job_that_used_to_publish_may_not_write_to_a_release(self) -> None:
        """One job writes, so an upload cannot race the check before publishing."""
        text = self.GOOD.replace(
            "  runtime:\n",
            "  publish-tarballs:\n    permissions:\n      contents: write\n  runtime:\n",
        )
        self.assertEqual(
            release_grant_problems(text),
            ["job publish-tarballs grants contents: write"],
        )

    def test_an_artifact_downloaded_over_the_checkout_is_refused(self) -> None:
        """It would replace the script a later step runs, with no tar involved."""
        for path in [
            "",
            "          path: datalib/tauri\n",
            "          path: ${{ github.workspace }}/x\n",
        ]:
            with self.subTest(path=path):
                text = self.GOOD.replace(
                    "          path: ${{ runner.temp }}/macos-app-compiled\n", path
                )
                problems = release_grant_problems(text)
                self.assertEqual(len(problems), 1, problems)
                self.assertIn("not under runner.temp", problems[0])

    def test_the_signing_job_never_unpacks_with_tar(self) -> None:
        """An unfiltered tar replaced build-signed-app.sh and the Tauri CLI, measured on bsdtar."""
        for command in [
            "tar -C datalib/tauri -xzf x.tar.gz",
            "tar xzf x.tar.gz",
            "tar --extract -xf x.tar",
        ]:
            with self.subTest(command=command):
                text = self.GOOD.replace(
                    "      - run: ./datalib/tauri/build-signed-app.sh sign\n",
                    f"      - run: {command}\n      - run: ./datalib/tauri/build-signed-app.sh sign\n",
                )
                problems = release_grant_problems(text)
                self.assertTrue(
                    any("unpacks with tar" in p for p in problems), problems
                )

    def test_the_signing_job_must_use_the_checked_unpack(self) -> None:
        text = self.GOOD.replace(
            "      - run: python3 -I scripts/release/unpack_compiled_app.py x datalib/tauri\n",
            "",
        )
        self.assertEqual(
            release_grant_problems(text),
            [
                "job macos-app-sign must unpack through scripts/release/unpack_compiled_app.py"
            ],
        )

    def test_a_release_names_no_remote_cache(self) -> None:
        """Bazel verifies no hit, so a cache `main` can write would decide what a release ships."""
        for line in [
            "      - run: bazel build --config=buildbuddy-release //x",
            "      - run: bazel build --remote_cache=grpcs://x //x",
            "      - run: cat > .bazelrc.user",
            "      - uses: actions/cache@abc",
            "      - run: docker buildx build --cache-from type=gha,scope=x ctx",
            "        env:\n          K: ${{ secrets.BUILD_BUDDY_RELEASE_API_KEY }}",
        ]:
            with self.subTest(line=line):
                text = self.GOOD.replace("      - run: bazel build //...", line)
                problems = release_grant_problems(text)
                self.assertEqual(len(problems), 1, problems)
                self.assertIn("a release reads no cache", problems[0])

    def test_the_release_orgs_key_stays_in_the_environment_main_alone_may_use(
        self,
    ) -> None:
        """As a plain repository secret, a workflow on any branch could name it."""
        key = "      - run: x\n        env:\n          K: ${{ secrets.BUILD_BUDDY_RELEASE_API_KEY }}\n"
        outside = "jobs:\n  test:\n    runs-on: x\n    steps:\n" + key
        self.assertEqual(
            release_key_problems(outside),
            [
                "job test names BUILD_BUDDY_RELEASE_API_KEY outside the main-ci environment"
            ],
        )
        inside = (
            "jobs:\n  test:\n    runs-on: x\n"
            "    environment: ${{ github.event_name == 'push' && 'main-ci' || '' }}\n"
            "    steps:\n" + key
        )
        self.assertEqual(release_key_problems(inside), [])
        self.assertEqual(release_key_problems("jobs:\n  test:\n    runs-on: x\n"), [])

    def test_a_third_party_image_is_pinned_by_digest(self) -> None:
        """Whoever controls a moved tag runs code in a job that can push the image."""
        text = """
# image=mirror.gcr.io/moby/buildkit:latest in a comment is fine
driver-opts: image=mirror.gcr.io/moby/buildkit:v1@sha256:abc
image: mirror.gcr.io/tonistiigi/binfmt:latest
driver-opts: image=mirror.gcr.io/moby/buildkit:v1
image: ghcr.io/imbue-ai/datalib_devcontainer:latest
"""
        self.assertEqual(
            floating_images(text),
            [
                "mirror.gcr.io/tonistiigi/binfmt:latest",
                "mirror.gcr.io/moby/buildkit:v1",
            ],
        )

    def test_a_comment_may_say_why_there_is_no_cache(self) -> None:
        text = self.GOOD.replace("# no bazel here", "# no BuildBuddy, no .bazelrc.user")
        self.assertEqual(release_grant_problems(text), [])

    def test_an_action_that_caches_says_it_does_not(self) -> None:
        """A tag's mac legs restored what `main`'s daily latchkey-auth saved; bazel checks no tree in it."""
        clean = self.GOOD.replace(
            "      - run: bazel build //...",
            "      - uses: bazel-contrib/setup-bazel@abc\n"
            "        with:\n"
            "          bazelisk-cache: false\n"
            "          disk-cache: false\n"
            "          repository-cache: false\n"
            "          external-cache: false\n"
            "      - uses: docker/setup-qemu-action@abc\n"
            "        with:\n"
            "          cache-image: false\n"
            "      - uses: docker/setup-buildx-action@abc\n"
            "        with:\n"
            "          cache-binary: false",
        )
        self.assertEqual(release_grant_problems(clean), [])
        for setting, action in [
            ("repository-cache", "setup-bazel"),
            ("cache-image", "setup-qemu-action"),
            ("cache-binary", "setup-buildx-action"),
        ]:
            expected = [f"a release reads no cache: {action} needs {setting}: false"]
            with self.subTest(setting=setting):
                turned_on = clean.replace(f"{setting}: false", f"{setting}: true")
                self.assertEqual(release_grant_problems(turned_on), expected)
                left_out = clean.replace(f"          {setting}: false\n", "")
                self.assertEqual(release_grant_problems(left_out), expected)


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
