"""What a launcher needs to stage a `DATALIB_RUNTIME_DIR` (see dev_runtime.sh).

The Bazel-managed Node and the two package stores, plus their locations
as runfiles paths — `rlocationpath` rather than a hand-written `_main/...`
string, so a moved target fails at analysis time instead of at
`rlocation` time. Every launcher that reaches dev_runtime.sh carries
both, because `env` applies only to the target `bazel run` was given,
not to a script it execs.
"""

DEV_RUNTIME_DATA = [
    "//datalib:dev_runtime.sh",
    "@nodejs_host//:node_bin",
    "//third-party/qmd/runtime:qmd_tree",
    "//third-party/qmd/runtime:package.json",
    "//third-party/qmd/runtime:qmd_package_dir",
    "//third-party/latchkey/runtime:latchkey_tree",
    "//third-party/latchkey/runtime:package.json",
    "//third-party/latchkey/runtime:latchkey_package_dir",
]

DEV_RUNTIME_ENV = {
    "DATALIB_DEV_NODE_BIN_RLOC": "$(rlocationpath @nodejs_host//:node_bin)",
    "DATALIB_DEV_QMD_PKG_RLOC": "$(rlocationpath //third-party/qmd/runtime:package.json)",
    "DATALIB_DEV_QMD_DIR_RLOC": "$(rlocationpath //third-party/qmd/runtime:qmd_package_dir)",
    "DATALIB_DEV_LATCHKEY_PKG_RLOC": "$(rlocationpath //third-party/latchkey/runtime:package.json)",
    "DATALIB_DEV_LATCHKEY_DIR_RLOC": "$(rlocationpath //third-party/latchkey/runtime:latchkey_package_dir)",
}
