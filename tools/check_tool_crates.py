#!/usr/bin/env python3
"""Fail when a crate is compiled a second time, for the exec ("tool")
configuration, for any reason other than a proc-macro or a build script.

A genrule's `tools` are built for the machine that runs the build, which
is a separate configuration from the one the tests and `:dist` use: a
Rust binary listed there is compiled again with everything under it.
Proc-macros and build scripts run at build time too, so the crates they
reach are expected; nothing else is. `docs/dev/ci.md` § "A `[for tool]`
suffix" has the fix — list the binary in the genrule's `srcs`.

    bazel aquery <the CI flags> --output=jsonproto \\
        'mnemonic("Rustc", deps(//...))' > aquery.json
    tools/check_tool_crates.py aquery.json
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from pathlib import Path

# Tools that are rules_rust's own plumbing, or code generators that run
# at build time by design. A repo named here is exempt as a whole.
_EXEMPT_REPOS = ("rules_rust+", "rules_rust_prost+")
_EXEMPT_LABELS = ("protoc-gen-prost__bin",)


@dataclass(frozen=True)
class Crate:
    label: str
    output: str
    crate_type: str
    externs: tuple[str, ...]
    is_tool: bool


def _repo(label: str) -> str:
    return label.lstrip("@").split("//", 1)[0]


def _is_expected_root(crate: Crate) -> bool:
    name = crate.label.rsplit(":", 1)[-1]
    return (
        crate.crate_type == "proc-macro"
        or name == "_bs_"
        or name.endswith("_build_script")
        or _repo(crate.label) in _EXEMPT_REPOS
        or name in _EXEMPT_LABELS
    )


def offenders(crates: list[Crate]) -> dict[str, list[str]]:
    """Tool-config crates no proc-macro or build script reaches, grouped
    under the tool binary (or unreached library) that pulls each in."""
    tools = [c for c in crates if c.is_tool]
    by_output = {c.output: c for c in tools}

    def closure(roots: list[Crate]) -> set[str]:
        seen: set[str] = set()
        todo = [c.output for c in roots]
        while todo:
            out = todo.pop()
            if out in seen or out not in by_output:
                continue
            seen.add(out)
            todo += by_output[out].externs
        return seen

    expected = closure([c for c in tools if _is_expected_root(c)])
    stray = [c for c in tools if c.output not in expected]
    reached_by_stray = {e for c in stray for e in c.externs}
    heads = [c for c in stray if c.output not in reached_by_stray]
    return {
        head.label: sorted(
            by_output[o].label for o in closure([head]) if o not in expected
        )
        for head in heads
    }


def parse(aquery: dict) -> list[Crate]:
    fragments = {f["id"]: f for f in aquery.get("pathFragments", [])}

    def path(fragment_id: int) -> str:
        parts = []
        while fragment_id:
            f = fragments[fragment_id]
            parts.append(f["label"])
            fragment_id = f.get("parentId", 0)
        return "/".join(reversed(parts))

    artifacts = {a["id"]: path(a["pathFragmentId"]) for a in aquery["artifacts"]}
    targets = {t["id"]: t["label"] for t in aquery["targets"]}
    tool_configs = {c["id"] for c in aquery["configuration"] if c.get("isTool", False)}
    crates = []
    for action in aquery["actions"]:
        if action.get("mnemonic") != "Rustc":
            continue
        args = [a.strip("'") for a in action.get("arguments", [])]
        crate_type = next(
            (a.split("=", 1)[1] for a in args if a.startswith("--crate-type=")), ""
        )
        externs = tuple(
            a.split("=", 2)[2]
            for a in args
            if a.startswith("--extern=") and a.count("=") >= 2
        )
        crates.append(
            Crate(
                label=targets[action["targetId"]],
                output=artifacts[action["primaryOutputId"]],
                crate_type=crate_type,
                externs=externs,
                is_tool=action["configurationId"] in tool_configs,
            )
        )
    return crates


def main(argv: list[str]) -> int:
    crates = parse(json.loads(Path(argv[1]).read_text()))
    found = offenders(crates)
    if not found:
        tools = sum(c.is_tool for c in crates)
        print(
            f"check_tool_crates: OK — {tools} tool-config crates, all reached by a proc-macro or build script"
        )
        return 0
    print(
        "check_tool_crates: crates compiled a second time, for the exec configuration:"
    )
    for head, labels in sorted(found.items()):
        print(f"\n  {head} pulls in {len(labels)}:")
        for label in labels:
            print(f"    {label}")
    print(
        "\nA Rust binary in a genrule's `tools` is built again for the exec "
        "configuration.\nList it in `srcs` instead; docs/dev/ci.md § "
        '"A `[for tool]` suffix" says why.'
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
