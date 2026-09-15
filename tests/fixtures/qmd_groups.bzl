"""The fixture groups that get vectors.

Every group is indexed for keyword search; only these are embedded, which
is what every vector-search test needs and a fraction of the CPU-only
embed the whole corpus costs. The list lives in a .bzl so a test in
another package can read it (`QMD_FIXTURE_EMBEDDED_GROUPS`) and expect a
document outside it to be indexed and not embedded — the case that makes
the grid's two columns two columns.
"""

QMD_EMBEDDED_GROUPS = [
    "slack",
    "claude-api",
]
