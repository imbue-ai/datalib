"""Shared JSONL I/O helpers.

The provider downloaders and the slack-side ingest parser all write/read
the same per-entity event-store JSONL format. Centralizing the reader
prevents the U+2028 / U+2029 record-shredding bug from regressing in any
one site — `json.dumps(..., ensure_ascii=False)` leaves those code points
unescaped inside string values, and `str.splitlines()` treats them as
line breaks. Python's file iterator only splits on `\\n`, which is the
only separator the writer emits.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    """Read a JSONL file, returning [] when missing.

    Iterates the file directly (never `read_text().splitlines()`) so that
    records containing unescaped U+2028 / U+2029 / \\r / \\v in string
    values are not shredded across record boundaries.
    """
    if not path.exists():
        return []
    with path.open() as f:
        return [json.loads(line) for line in f if line.strip()]
