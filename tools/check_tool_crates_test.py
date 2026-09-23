"""check_tool_crates.py's rule, on hand-built graphs and one aquery shape."""

from __future__ import annotations

import unittest

from check_tool_crates import Crate, offenders, parse


def crate(label: str, *externs: str, kind: str = "rlib", tool: bool = True) -> Crate:
    return Crate(label, f"out/{label}", kind, tuple(f"out/{e}" for e in externs), tool)


class OffendersTest(unittest.TestCase):
    def test_what_a_proc_macro_or_build_script_reaches_is_expected(self) -> None:
        crates = [
            crate("@x//:serde_derive", "@x//:syn", kind="proc-macro"),
            crate("@x//:syn", "@x//:proc_macro2"),
            crate("@x//:proc_macro2"),
            crate("@y//:_bs_", "@x//:cc", kind="bin"),
            crate("@x//:cc"),
        ]
        self.assertEqual(offenders(crates), {})

    def test_a_tool_binary_names_every_crate_it_alone_pulls_in(self) -> None:
        """The regression: a genrule's Rust tool rebuilt its whole closure."""
        crates = [
            crate("//a:indexer_bin", "//a:indexer", kind="bin"),
            crate("//a:indexer", "@x//:tokio", "@x//:syn"),
            crate("@x//:tokio"),
            crate("@x//:serde_derive", "@x//:syn", kind="proc-macro"),
            crate("@x//:syn"),
        ]
        self.assertEqual(
            offenders(crates),
            {"//a:indexer_bin": ["//a:indexer", "//a:indexer_bin", "@x//:tokio"]},
        )

    def test_target_config_crates_and_rules_rust_plumbing_are_ignored(self) -> None:
        crates = [
            crate("//a:indexer_bin", "//a:indexer", kind="bin", tool=False),
            crate("//a:indexer", tool=False),
            crate("@@rules_rust+//util/process_wrapper:process_wrapper", kind="bin"),
        ]
        self.assertEqual(offenders(crates), {})


class ParseTest(unittest.TestCase):
    def test_reads_label_output_type_externs_and_configuration(self) -> None:
        aquery = {
            "pathFragments": [
                {"id": 1, "label": "bazel-out"},
                {"id": 2, "label": "libtokio.rlib", "parentId": 1},
                {"id": 3, "label": "libbytes.rlib", "parentId": 1},
            ],
            "artifacts": [{"id": 10, "pathFragmentId": 2}],
            "targets": [{"id": 5, "label": "@x//:tokio"}],
            "configuration": [
                {"id": 1, "mnemonic": "k8-opt"},
                {"id": 2, "mnemonic": "k8-opt-exec", "isTool": True},
            ],
            "actions": [
                {
                    "mnemonic": "Rustc",
                    "targetId": 5,
                    "configurationId": 2,
                    "primaryOutputId": 10,
                    "arguments": [
                        "process_wrapper",
                        "--crate-type=rlib'",
                        "--extern=bytes=bazel-out/libbytes.rlib'",
                    ],
                },
                {"mnemonic": "CppCompile", "targetId": 5, "configurationId": 1},
            ],
        }
        self.assertEqual(
            parse(aquery),
            [
                Crate(
                    "@x//:tokio",
                    "bazel-out/libtokio.rlib",
                    "rlib",
                    ("bazel-out/libbytes.rlib",),
                    True,
                )
            ],
        )


if __name__ == "__main__":
    unittest.main()
