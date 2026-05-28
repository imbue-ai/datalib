"""Helper for wiring up `bazel run //pkg:foo.update` snapshot-update
targets next to insta-using `rust_test`s.

Usage from a provider's `BUILD.bazel`:

    load("//tools:insta.bzl", "insta_update")

    rust_test(
        name = "chatgpt_render",
        srcs = ["tests/chatgpt_render.rs"],
        ...
    )

    insta_update(
        name = "chatgpt_render.update",
        test = ":chatgpt_render",
    )

Then `bazel run //frankweiler/backend/etl/providers/chatgpt:chatgpt_render.update`
re-runs the test with `INSTA_UPDATE=always` and `INSTA_WORKSPACE_ROOT=$BUILD_WORKSPACE_DIRECTORY`,
which is the standard insta-with-bazel idiom: insta resolves snapshot
paths against the user's actual workspace, not the bazel sandbox.

When the underlying `rust_test` uses additional `data` deps + `env` vars
(e.g. //frankweiler/backend/sync:manual_e2e_live_sync_golden, which
reaches the sync binary through `FRANKWEILER_SYNC_BIN`), pass them via
`extra_data` + `extra_env` so the `.update` target picks them up too —
`rust_test`'s env doesn't propagate to a sibling sh_binary.
"""

load("@rules_shell//shell:sh_binary.bzl", "sh_binary")

def insta_update(
        name,
        test,
        test_args = None,
        extra_data = None,
        extra_env = None,
        visibility = None):
    """Generates a `bazel run`-able sibling target that updates snapshots.

    Args:
      name: target name (convention: `<test>.update`).
      test: the `rust_test` label whose snapshots to update.
      test_args: optional extra args passed verbatim to the test
        binary (e.g. `["--ignored"]` for `#[ignore]`d tests).
      extra_data: optional extra `data` deps. Mirrors the underlying
        `rust_test`'s `data =` when the test reaches dependencies via
        `$(rootpath ...)`-style env vars.
      extra_env: optional extra env vars (Make-variable interpolation
        supported, e.g. `"$(rootpath :foo_bin)"`). Mirrors the
        underlying `rust_test`'s `env =`.
      visibility: optional visibility list.
    """
    args_str = " ".join(test_args) if test_args else ""

    env = {
        "INSTA_TEST_BIN": "$(rootpath {})".format(test),
        "INSTA_TEST_ARGS": args_str,
    }
    if extra_env:
        env.update(extra_env)

    data = [test]
    if extra_data:
        data = data + list(extra_data)

    sh_binary(
        name = name,
        srcs = ["//tools:insta_update.sh"],
        data = data,
        env = env,
        # We depend on a `rust_test` target, which is `testonly`. Mark
        # the wrapper testonly too so bazel's dependency-correctness
        # check doesn't reject it.
        testonly = True,
        visibility = visibility,
    )
