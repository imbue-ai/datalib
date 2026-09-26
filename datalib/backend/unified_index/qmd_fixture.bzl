"""What a rust_test linking `//datalib/backend/unified_index:qmd_fixture` needs.

The fixture's qmd and grid indexes, and the node, qmd package and embedding
model it runs qmd with, as `data`; and the runfile paths of the last three
as `env`, named by bazel because their repositories' names carry the host
platform.
"""

QMD_FIXTURE_DATA = [
    "//tests/fixtures:ingested/backend_index.doltlite_db",
    "//tests/fixtures:ingested/qmd.tar",
    "//tests/fixtures:ingested/qmd-index.tar",
    "@nodejs_host//:node_bin",
    "//third-party/qmd/runtime:qmd_package_dir",
    "//third-party/qmd/runtime:qmd_tree",
    "//third-party/qmd_models:embeddinggemma",
]

QMD_FIXTURE_ENV = {
    "QMD_FIXTURE_EMBED_MODEL_RLOC": "$(rlocationpath //third-party/qmd_models:embeddinggemma)",
    "QMD_FIXTURE_NODE_RLOC": "$(rlocationpath @nodejs_host//:node_bin)",
    "QMD_FIXTURE_QMD_DIR_RLOC": "$(rlocationpath //third-party/qmd/runtime:qmd_package_dir)",
}
