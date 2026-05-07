"""Syrupy extensions that store golden files as plain text.

Default syrupy uses an `.ambr` format (one file, many snapshots, custom
framing). That's compact, but it isn't viewable on its own — you can't
preview the rendered Markdown of a `.qmd` golden, nor open the `.sql`
golden in a SQL editor.

These extensions subclass `SingleFileSnapshotExtension` so each snapshot
gets its own file with a familiar extension (`.md` or `.sql`) and *no*
syrupy framing — the file's contents are exactly the bytes asserted on.
That means:

  * The `.md` goldens render natively in any Markdown viewer (GitHub,
    IDE preview, Quarto). Useful for eyeballing ingestion output.
  * The `.sql` goldens load straight into SQLite/MySQL clients.
  * Diffs in code review are line-level over real content.
"""

from __future__ import annotations

from syrupy.extensions.single_file import SingleFileSnapshotExtension, WriteMode


class _PlainTextSingleFileExtension(SingleFileSnapshotExtension):
    """Base: write/read snapshots as raw text, no syrupy header."""

    _write_mode = WriteMode.TEXT

    def serialize(self, data, **kwargs) -> str:  # type: ignore[override]
        if isinstance(data, bytes):
            return data.decode("utf-8")
        return str(data)


class MarkdownSnapshotExtension(_PlainTextSingleFileExtension):
    file_extension = "md"


class SqlSnapshotExtension(_PlainTextSingleFileExtension):
    file_extension = "sql"
