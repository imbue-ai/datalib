"""Helper for wiring up `bazel run //pkg:foo_live` targets beside the
`rust_test` whose `live` module holds them.

A live test talks to a real service through `latchkey`. It is not a
separate binary: it is a module named `live` inside the package's one
test binary, so it is compiled with everything else (and cannot rot)
while the package's `rust_test` skips it by name:

    rust_test(
        name = "claude_tests",
        args = ["--skip", "live::"],
        ...
    )

    live_run(
        name = "claude_live",
        test = ":claude_tests",
    )

Then `bazel run //datalib/backend/etl/providers/claude:claude_live`
runs exactly what that `--skip` excluded, with the invoking shell's
environment — which is what these tests need, and what a `bazel test`
of the same target could neither provide nor un-skip.

`#[ignore]` is deliberately *not* how this works. It is one global
flag, and `insta_update`'s `test_args` already spends it on snapshot
tests that are genuinely ignored; a second meaning would make the two
impossible to tell apart in one binary.
"""

load("@rules_shell//shell:sh_binary.bzl", "sh_binary")

def live_run(
        name,
        test,
        filter = "live::",
        extra_data = None,
        extra_env = None,
        visibility = None):
    """Generates a `bazel run`-able target for a test binary's live module.

    Args:
      name: target name (convention: `<provider>_live`).
      test: the `rust_test` whose `live` module to run.
      filter: the libtest name filter; the default matches the module.
      extra_data: optional extra `data` deps, mirroring the underlying
        `rust_test`'s `data =` when it reaches them through
        `$(rootpath ...)`-style env vars.
      extra_env: optional extra env vars, same reason.
      visibility: optional visibility list.
    """
    env = {
        "LIVE_TEST_BIN": "$(rootpath {})".format(test),
        "LIVE_TEST_FILTER": filter,
    }
    if extra_env:
        env.update(extra_env)

    data = [test]
    if extra_data:
        data = data + list(extra_data)

    sh_binary(
        name = name,
        srcs = ["//tools:live_run.sh"],
        data = data,
        env = env,
        # Depends on a `rust_test`, which is `testonly`.
        testonly = True,
        visibility = visibility,
    )
