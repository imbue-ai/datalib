"""One module of a `rust_test` binary, run as a test target of its own.

A package's integration tests share one binary, because the link is
most of what a Rust test costs (docs/dev/testing.md). A module that
needs a process of its own — it installs the process's only tracing
subscriber — or tags the rest must not carry — `no-sandbox` — still
compiles into that binary. The package's test skips it by name, and a
slice runs exactly that module, from the same binary, in its own
process, with its own tags, data and env:

    _SLICES = {"server_log_test": "server_log::"}

    rust_test(
        name = "http_tests",
        args = skip_slices(_SLICES),
        ...
    )

    rust_test_slice(
        name = "server_log_test",
        filter = _SLICES["server_log_test"],
        test = ":http_tests",
    )

The slice fails when its filter matches no test, so a renamed module
cannot turn it into a green no-op. The same trick, run by hand, is
`live.bzl`.
"""

load("@rules_shell//shell:sh_test.bzl", "sh_test")

def skip_slices(slices):
    """`--skip` arguments for every filter in a `{name: filter}` dict."""
    args = []
    for f in slices.values():
        args += ["--skip", f]
    return args

def rust_test_slice(name, test, filter, data = None, env = None, **kwargs):
    """Runs the tests of `test` whose names contain `filter`.

    Args:
      name: the slice's target name.
      test: the `rust_test` whose binary holds the module.
      filter: a libtest name filter, conventionally `<module>::`.
      data: runfiles the module needs beyond `test`'s own.
      env: env the module reads; `$(rootpath)` of `data` expands.
      **kwargs: `sh_test` attributes: `size`, `tags`, `timeout`, ...
    """
    slice_env = {
        "TEST_SLICE_BIN": "$(rootpath {})".format(test),
        "TEST_SLICE_FILTER": filter,
    }
    slice_env.update(env or {})
    sh_test(
        name = name,
        srcs = ["//tools:test_slice.sh"],
        data = [test] + (data or []),
        env = slice_env,
        **kwargs
    )
