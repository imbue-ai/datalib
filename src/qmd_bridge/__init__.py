"""Bridge between qmd semantic-search hits and grid_rows."""

from qmd_bridge.mapping import (
    GridIndex,
    GridRowRef,
    QmdHit,
    QueryMode,
    parse_query,
)

__all__ = [
    "GridIndex",
    "GridRowRef",
    "QmdHit",
    "QueryMode",
    "parse_query",
]
