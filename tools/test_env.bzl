"""`run_with_test_env`: a `bazel run` target that runs a script beside a
`rust_test` with that test's own `env` (as `rust_test` publishes it in
`RunEnvironmentInfo`, its `$(rootpath …)`s expanded) and its own
runfiles, so a wrapper never lists what the test needs a second time.
Behind `insta_update` and `live_run`.
"""

def _shell_quote(s):
    return "'" + s.replace("'", "'\\''") + "'"

def _run_with_test_env_impl(ctx):
    test = ctx.executable.test
    script = ctx.file.script
    env = dict(ctx.attr.test[RunEnvironmentInfo].environment)
    env.update(ctx.attr.env)
    env[ctx.attr.test_bin_var] = test.short_path
    lines = ["#!/usr/bin/env bash", "set -euo pipefail"]
    lines += ["export {}={}".format(k, _shell_quote(v)) for k, v in sorted(env.items())]
    lines.append('exec {} "$@"'.format(_shell_quote(script.short_path)))
    launcher = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(launcher, "\n".join(lines) + "\n", is_executable = True)
    runfiles = ctx.runfiles(files = [test, script]).merge(
        ctx.attr.test[DefaultInfo].default_runfiles,
    )
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

_run_with_test_env = rule(
    implementation = _run_with_test_env_impl,
    executable = True,
    attrs = {
        "env": attr.string_dict(doc = "What the script reads, on top of the test's env."),
        "script": attr.label(mandatory = True, allow_single_file = True),
        "test": attr.label(
            doc = "The `rust_test` to run.",
            executable = True,
            cfg = "target",
            mandatory = True,
            providers = [RunEnvironmentInfo],
        ),
        "test_bin_var": attr.string(
            doc = "The variable the script finds the test binary's path in.",
            mandatory = True,
        ),
    },
)

def run_with_test_env(name, test, script, test_bin_var, env = {}, visibility = None):
    _run_with_test_env(
        name = name,
        test = test,
        script = script,
        test_bin_var = test_bin_var,
        env = env,
        # A `rust_test` is `testonly`, and so is whatever depends on it.
        testonly = True,
        visibility = visibility,
    )
