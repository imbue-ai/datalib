"""`bazel run //pkg:foo.update`: re-runs an insta-using `rust_test` so its
snapshots are written into the source tree (`tools/insta_update.sh`),
with the test's own env and runfiles (`tools/test_env.bzl`).

    load("//tools:insta.bzl", "insta_update")

    insta_update(
        name = "chatgpt_tests.update",
        test = ":chatgpt_tests",
        test_args = ["--skip", "live::"],
    )
"""

load("//tools:test_env.bzl", "run_with_test_env")

def insta_update(name, test, test_args = [], visibility = None):
    run_with_test_env(
        name = name,
        test = test,
        script = "//tools:insta_update.sh",
        test_bin_var = "INSTA_TEST_BIN",
        env = {"INSTA_TEST_ARGS": " ".join(test_args)},
        visibility = visibility,
    )
