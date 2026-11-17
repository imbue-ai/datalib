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
runs exactly what that `--skip` excluded, with the test's own env
and runfiles (`tools/test_env.bzl`) and the invoking shell's
environment — which is what these tests need, and what a `bazel test`
of the same target could neither provide nor un-skip.

`#[ignore]` is deliberately *not* how this works. It is one global
flag, and `insta_update`'s `test_args` already spends it on snapshot
tests that are genuinely ignored; a second meaning would make the two
impossible to tell apart in one binary.
"""

load("//tools:test_env.bzl", "run_with_test_env")

def live_run(name, test, filter = "live::", visibility = None):
    run_with_test_env(
        name = name,
        test = test,
        script = "//tools:live_run.sh",
        test_bin_var = "LIVE_TEST_BIN",
        env = {"LIVE_TEST_FILTER": filter},
        visibility = visibility,
    )
