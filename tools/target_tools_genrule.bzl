"""A genrule whose `tools` are built for the target configuration.

`genrule` configures its `tools` for the *exec* platform, which is right
for a code generator and wrong for a pipeline binary that is also what the
tests link and `:dist` ships: bazel then builds that binary — and every
crate under it — a second time under `bazel-out/<cpu>-opt-exec/`, with
nothing shared between the two copies. `//tests/fixtures:ingested_tng`
names `datalib_step`, which pulls in the whole backend, so the cost was
one extra compile of the tree per PR. Here the tools are the same
configured targets the tests use, which is sound because they run on the
platform they are built for (the musl static builds included).

Supports what that rule needs and nothing more: `$(execpath …)` and
`$(RULEDIR)` in `cmd`, run under bash with the default shell environment.
"""

def _target_tools_genrule_impl(ctx):
    ruledir = ctx.bin_dir.path + "/" + ctx.label.package
    cmd = ctx.expand_location(ctx.attr.cmd, ctx.attr.srcs + ctx.attr.tools)
    cmd = cmd.replace("$(RULEDIR)", ruledir)
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        tools = [t[DefaultInfo].files_to_run for t in ctx.attr.tools],
        outputs = ctx.outputs.outs,
        command = cmd,
        mnemonic = "TargetToolsGenrule",
        progress_message = "Executing target-tools genrule %{label}",
        use_default_shell_env = True,
    )
    return [DefaultInfo(files = depset(ctx.outputs.outs))]

target_tools_genrule = rule(
    implementation = _target_tools_genrule_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "outs": attr.output_list(mandatory = True),
        "tools": attr.label_list(allow_files = True, cfg = "target"),
        "cmd": attr.string(mandatory = True),
    },
)
