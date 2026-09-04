#!/usr/bin/env python3
"""Side-by-side diff of two `fsindex` directory scans, as one HTML page.

Prototype. See `README.md` in this directory for the findings that
motivated the design — in particular *why* two scans that live in
separate `.doltlite_db` files get unified into a scratch database
before anything is diffed.

The short version: doltlite's prolly-tree diff
(`dolt_diff_files(<from>, <to>)`) is only available on the *main*
database of a connection, and it resolves commit hashes against that
database's own chunk store. `ATTACH` does not extend either. But a
doltlite file can act as a `file://` remote for another, so fetching
both scans into one throwaway database makes the two histories
resolvable side by side without touching either original.

Move detection rides on `fsindex`'s directory tree-hashes. A directory
row's `blake3` covers its whole subtree (see the provider's
`schema_raw.rs` §"Directory tree-hash canonicalization"), so a moved
directory keeps its hash and shows up as a `removed` row and an `added`
row carrying the same digest. Pairing those two is the move. That is
the thing Unison does not do — it reports the delete and the create and
leaves you to notice they are the same bytes.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# A hex digest as doltlite's `hex()` renders it. Used to build IN-lists
# by string concatenation, so it is validated rather than trusted: the
# doltlite CLI takes SQL as an argv string with no bind parameters.
HEX_RE = re.compile(r"\A[0-9A-F]*\Z")

STATUS_ORDER = (
    "moved_out",
    "moved_in",
    "removed",
    "removed_but_copy_remains",
    "added",
    "added_from_copy",
    "modified",
    "structure",
    "unchanged",
)


class DoltliteError(RuntimeError):
    """A doltlite invocation failed, with its stderr attached."""


# ---------------------------------------------------------------------
# doltlite plumbing
# ---------------------------------------------------------------------


def find_doltlite(explicit: str | None) -> str:
    """Locate the Bazel-built doltlite shell.

    Prefers an explicit path, then `$DOLTLITE`, then the Bazel output
    tree. The host `/usr/local/bin/doltlite` is deliberately *not*
    consulted: it can be a different version than `MODULE.bazel` pins,
    which is the one way this tool could silently disagree with what
    the pipeline wrote.
    """
    if explicit:
        return explicit
    env = os.environ.get("DOLTLITE")
    if env:
        return env
    here = Path(__file__).resolve()
    for parent in here.parents:
        cand = parent / "bazel-bin" / "third-party" / "doltlite" / "doltlite"
        if cand.exists():
            return str(cand)
    raise SystemExit(
        "could not find the doltlite shell. Build it with\n"
        "  bazelisk build //third-party/doltlite:doltlite\n"
        "or pass --doltlite /path/to/doltlite"
    )


@dataclass
class Dolt:
    """A thin subprocess wrapper around the doltlite shell."""

    binary: str

    def rows(self, db: str, sql: str) -> list[dict[str, object]]:
        """Run `sql` against `db` and decode the `-json` result set."""
        proc = subprocess.run(
            [self.binary, "-json", db, sql],
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            raise DoltliteError(f"{db}: {proc.stderr.strip()}\n  sql: {sql[:400]}")
        out = proc.stdout.strip()
        if not out:
            # doltlite prints nothing at all for an empty result set.
            return []
        parsed = json.loads(out)
        if not isinstance(parsed, list):
            raise DoltliteError(f"{db}: expected a JSON array, got {type(parsed)}")
        return parsed

    def script(self, db: str, statements: list[str]) -> None:
        """Run statements one at a time.

        The shell emits one JSON document per statement, so a single
        multi-statement argument comes back as several concatenated
        arrays that `json.loads` rejects. Sending them separately keeps
        every result decodable.
        """
        for statement in statements:
            self.rows(db, statement)

    def scalar(self, db: str, sql: str) -> object:
        rows = self.rows(db, sql)
        if not rows:
            raise DoltliteError(f"{db}: expected one row, got none\n  sql: {sql}")
        return next(iter(rows[0].values()))


def resolve_ref(dolt: Dolt, db: str, ref: str) -> str:
    """Resolve a branch name / `HEAD~2` / raw hash to a commit hash.

    Done inside the scan's *own* file, before unification, because that
    is the only database where the ref's name is meaningful.
    """
    value = dolt.scalar(db, f"SELECT dolt_hashof('{sql_literal(ref)}');")
    return str(value)


def sql_literal(text: str) -> str:
    """Escape a string for a single-quoted SQL literal."""
    return text.replace("'", "''")


# ---------------------------------------------------------------------
# model
# ---------------------------------------------------------------------


def as_int(value: object) -> int:
    """Coerce one decoded JSON cell to an int, treating NULL as zero."""
    if value is None or value == "":
        return 0
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (int, float)):
        return int(value)
    return int(str(value))


def as_str(value: object) -> str:
    """Coerce one decoded JSON cell to a string, treating NULL as empty."""
    return "" if value is None else str(value)


@dataclass
class Entry:
    """One row of `files`, on one side of the comparison."""

    path: str
    kind: str
    size: int
    digest: str


@dataclass
class Node:
    """A rendered tree node on one side."""

    path: str
    kind: str
    size: int
    status: str
    peer: str | None = None
    note: str = ""
    rolled_up: int = 0
    dup: dict[str, object] | None = None


@dataclass
class Diff:
    removed: list[Entry] = field(default_factory=list)
    added: list[Entry] = field(default_factory=list)
    modified: list[tuple[Entry, Entry]] = field(default_factory=list)


# ---------------------------------------------------------------------
# fetching the diff
# ---------------------------------------------------------------------


def unify(dolt: Dolt, left_db: str, right_db: str, workdir: Path) -> str:
    """Fetch two independent scan files into one scratch database.

    Neither input is opened for writing or copied; each is added as a
    read-only `file://` remote of a brand-new database, which is what
    makes the two commit graphs resolvable in a single connection.
    Chunk-level dedup means this costs roughly the *novelty* between
    the two scans rather than the size of the second one.
    """
    scratch = workdir / "unified.doltlite_db"
    left_abs = Path(left_db).resolve()
    right_abs = Path(right_db).resolve()
    dolt.script(
        str(scratch),
        [
            f"SELECT dolt_remote('add','left','file://{sql_literal(str(left_abs))}');",
            f"SELECT dolt_remote('add','right','file://{sql_literal(str(right_abs))}');",
            "SELECT dolt_fetch('left');",
            "SELECT dolt_fetch('right');",
        ],
    )
    return str(scratch)


DIFF_SQL = """SELECT diff_type,
       from_id, to_id,
       from_kind, to_kind,
       from_size, to_size,
       hex(from_blake3) AS from_hash,
       hex(to_blake3)   AS to_hash
FROM dolt_diff_files('{frm}','{to}')"""


def fetch_diff(dolt: Dolt, db: str, from_hash: str, to_hash: str) -> Diff:
    rows = dolt.rows(db, DIFF_SQL.format(frm=from_hash, to=to_hash) + ";")
    diff = Diff()
    for row in rows:
        kind = str(row["diff_type"])
        if kind == "removed":
            diff.removed.append(entry_from(row, "from"))
        elif kind == "added":
            diff.added.append(entry_from(row, "to"))
        elif kind == "modified":
            diff.modified.append((entry_from(row, "from"), entry_from(row, "to")))
        else:
            raise DoltliteError(f"unexpected diff_type {kind!r}")
    return diff


def entry_from(row: dict[str, object], side: str) -> Entry:
    return Entry(
        path=as_str(row[f"{side}_id"]),
        kind=as_str(row[f"{side}_kind"]),
        size=as_int(row[f"{side}_size"]),
        digest=as_str(row[f"{side}_hash"]),
    )


def lookup_digests(
    dolt: Dolt, db: str, commit: str, digests: set[str], verbose: bool
) -> dict[str, str]:
    """Find where each of `digests` lives in the tree at `commit`.

    This is the one deliberately expensive query in the tool. `files`
    carries no secondary index on `blake3` (the provider documents why
    in `schema_raw.rs`), so each chunk is a whole-corpus scan. It runs
    only for digests the diff could not already account for, and it
    announces itself — a quiet O(corpus) scan hiding behind a fast
    O(changes) diff is exactly the kind of fallback this repo tells you
    not to add silently.
    """
    if not digests:
        return {}
    for digest in digests:
        if not HEX_RE.match(digest):
            raise DoltliteError(f"refusing to interpolate non-hex digest {digest!r}")
    if verbose:
        print(
            f"note: scanning the corpus at {commit[:10]} for "
            f"{len(digests)} unmatched digest(s) — `files` has no blake3 "
            "index, so this is a full scan per chunk",
            file=sys.stderr,
        )
    found: dict[str, str] = {}
    ordered = sorted(digests)
    for start in range(0, len(ordered), 400):
        chunk = ordered[start : start + 400]
        in_list = ",".join(f"'{d}'" for d in chunk)
        # Safe by construction: every element matched HEX_RE above, so
        # the list holds only [0-9A-F] characters inside quotes.
        rows = dolt.rows(
            db,
            f"SELECT id, hex(blake3) AS h FROM dolt_at_files('{commit}') "
            f"WHERE hex(blake3) IN ({in_list});",
        )
        for row in rows:
            digest = str(row["h"])
            path = str(row["id"] or "")
            found.setdefault(digest, path)
    return found


def load_side(dolt: Dolt, db: str, commit: str) -> dict[str, Entry]:
    """Every row of `files` at `commit`. Only for `--full-tree`."""
    rows = dolt.rows(
        db,
        f"SELECT id, kind, size, hex(blake3) AS h FROM dolt_at_files('{commit}');",
    )
    out: dict[str, Entry] = {}
    for row in rows:
        path = as_str(row["id"])
        out[path] = Entry(
            path=path,
            kind=as_str(row["kind"]),
            size=as_int(row["size"]),
            digest=as_str(row["h"]),
        )
    return out


# ---------------------------------------------------------------------
# duplicates within a single tree
# ---------------------------------------------------------------------


@dataclass
class DupGroup:
    """Two or more paths in ONE tree holding byte-identical content."""

    digest: str
    kind: str
    size: int
    paths: list[str]
    rolled_up: int = 0

    @property
    def wasted(self) -> int:
        """Bytes that would come back if every copy but one went away."""
        return (len(self.paths) - 1) * self.size


def parse_size(text: str) -> int:
    """Parse a byte threshold, accepting `4096`, `64K`, `1M`, `2G`."""
    raw = text.strip().upper().rstrip("B")
    if not raw:
        raise argparse.ArgumentTypeError("empty size")
    scale = {"K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4}
    factor = 1
    if raw[-1] in scale:
        factor = scale[raw[-1]]
        raw = raw[:-1]
    try:
        return int(float(raw) * factor)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"not a byte size: {text!r}") from exc


def duplicate_candidates(
    dolt: Dolt, db: str, commit: str, threshold: int, verbose: bool
) -> list[Entry]:
    """Read the entries of one tree that are big enough to be worth
    checking for duplication. I/O only — the grouping is
    [`group_duplicates`], which is pure and is where the tests live.

    Costs a full scan of the tree. `files` has no index on `size`, so
    the `>= threshold` filter saves transfer and grouping work, not the
    scan itself.
    """
    if threshold <= 0:
        return []
    if verbose:
        print(
            f"note: scanning the tree at {commit[:10]} for duplicate content "
            f">= {threshold} bytes — this is a full corpus scan",
            file=sys.stderr,
        )
    rows = dolt.rows(
        db,
        "SELECT id, kind, size, hex(blake3) AS h FROM "
        f"dolt_at_files('{commit}') WHERE size >= {int(threshold)};",
    )
    return [
        Entry(
            path=as_str(row["id"]),
            kind=as_str(row["kind"]),
            size=as_int(row["size"]),
            digest=as_str(row["h"]),
        )
        for row in rows
        if as_str(row["id"])
    ]


def group_duplicates(entries: list[Entry]) -> list[DupGroup]:
    """Group one tree's entries by digest, keeping only the repeats.

    Answers a question the left/right diff cannot: *is this tree storing
    the same bytes more than once?* A directory counts, because its
    digest covers its whole subtree — so a folder copied to a second
    place inside the same tree is one finding, not one per file in it.

    Pure: hand it a list of entries and it returns the groups, which is
    what makes the rollup behaviour testable without a database.
    """
    buckets: dict[tuple[str, str], list[Entry]] = {}
    for entry in entries:
        if not entry.path:
            continue
        buckets.setdefault((entry.kind, entry.digest), []).append(entry)

    groups: list[DupGroup] = []
    for (kind, digest), members in buckets.items():
        if len(members) < 2:
            continue
        members.sort(key=lambda e: (depth(e.path), e.path))
        groups.append(
            DupGroup(
                digest=digest,
                kind=kind,
                size=members[0].size,
                paths=[e.path for e in members],
            )
        )
    return roll_up_duplicates(groups)


def roll_up_duplicates(groups: list[DupGroup]) -> list[DupGroup]:
    """Drop duplicate groups that a duplicated parent directory implies.

    If `themes/dark` and `themes/dark_backup` are the same directory,
    every file inside them is trivially duplicated too. Only the
    outermost pair is worth reporting — the same rule the move and copy
    rollups follow, run over the same [`roll_up`] machinery by linking
    each repeat back to the shallowest member of its group.
    """
    links: list[Link] = []
    owner: list[DupGroup] = []
    for group in groups:
        canonical = group.paths[0]
        for other in group.paths[1:]:
            links.append(Link(canonical, other, group.kind))
            owner.append(group)
    roll_up(links)

    survivors: dict[int, DupGroup] = {}
    rolled: dict[int, int] = {}
    for link, group in zip(links, owner):
        if link.covered:
            continue
        survivors[id(group)] = group
        rolled[id(group)] = rolled.get(id(group), 0) + link.rolled_up
    for key, group in survivors.items():
        group.rolled_up = rolled.get(key, 0)
    return sorted(survivors.values(), key=lambda g: -g.wasted)


# ---------------------------------------------------------------------
# classification
# ---------------------------------------------------------------------


@dataclass
class Move:
    src: Entry
    dst: Entry
    rolled_up: int = 0


def basename(path: str) -> str:
    return path.rsplit("/", 1)[-1]


def depth(path: str) -> int:
    return 0 if not path else path.count("/") + 1


def pair_moves(diff: Diff) -> tuple[list[Move], list[Entry], list[Entry]]:
    """Match removed rows against added rows carrying the same digest.

    A pair is a move: the same bytes, at a different path. What is left
    over on the removed side is a real disappearance from this tree, and
    what is left over on the added side is genuinely new content —
    subject to the copy check that runs afterwards.

    Pairing is greedy and prefers a candidate that kept its basename,
    which is what makes `docs/reports` pair with `archive/reports`
    rather than with some unrelated directory that happens to hold
    identical content.
    """
    removed_by: dict[tuple[str, str], list[Entry]] = {}
    added_by: dict[tuple[str, str], list[Entry]] = {}
    for entry in diff.removed:
        if entry.path:
            removed_by.setdefault((entry.kind, entry.digest), []).append(entry)
    for entry in diff.added:
        if entry.path:
            added_by.setdefault((entry.kind, entry.digest), []).append(entry)

    moves: list[Move] = []
    used_removed: set[str] = set()
    used_added: set[str] = set()
    for key, sources in removed_by.items():
        targets = added_by.get(key)
        if not targets:
            continue
        pool = list(targets)
        for src in sources:
            if not pool:
                break
            pool.sort(
                key=lambda dst, s=src: (
                    basename(dst.path) != basename(s.path),
                    abs(depth(dst.path) - depth(s.path)),
                    dst.path,
                )
            )
            dst = pool.pop(0)
            moves.append(Move(src=src, dst=dst))
            used_removed.add(src.path)
            used_added.add(dst.path)

    residual_removed = [
        e for e in diff.removed if e.path and e.path not in used_removed
    ]
    residual_added = [e for e in diff.added if e.path and e.path not in used_added]
    return moves, residual_removed, residual_added


@dataclass
class Link:
    """A correspondence between one path on the left and one on the right.

    Covers both kinds of "these are the same bytes elsewhere" relation
    the viewer reports: a move (gone from the left, present on the
    right) and a copy (present on both, at a path that is new or newly
    absent on one side).
    """

    src: str
    dst: str
    kind: str
    rolled_up: int = 0
    covered: bool = False


def covering(accepted: list[Link], link: Link) -> Link | None:
    """The outermost accepted directory link that already implies `link`."""
    for parent in accepted:
        if parent is link or parent.dst == link.dst:
            continue
        if not link.dst.startswith(parent.dst + "/"):
            continue
        suffix = link.dst[len(parent.dst) :]
        if link.src == parent.src + suffix:
            return parent
    return None


def roll_up(links: list[Link]) -> None:
    """Collapse a related subtree into the single outermost directory.

    Moving `docs/` to `archive/` moves every descendant with it, and
    copying a directory copies every descendant with it. Either way each
    descendant arrives here as its own link, and reporting all of them
    buries the one fact worth reading. So a link is marked `covered`
    when an ancestor directory made exactly the same journey — the same
    relative suffix on both sides — and the surviving ancestor carries a
    count of what it absorbed.

    Covered links are never rendered as their own finding. In
    `--full-tree` mode their paths still appear, as ordinary unchanged
    entries under the directory that moved; in the default mode they are
    simply absent.
    """
    dir_links = sorted(
        (link for link in links if link.kind == "dir"), key=lambda link: depth(link.dst)
    )
    accepted: list[Link] = []
    for candidate in dir_links:
        if covering(accepted, candidate) is None:
            accepted.append(candidate)
    for link in links:
        parent = covering(accepted, link)
        if parent is not None:
            parent.rolled_up += 1
            link.covered = True


@dataclass
class Finding:
    """One reportable row on one side of the comparison."""

    entry: Entry
    status: str
    peer: str | None = None
    note: str = ""
    rolled_up: int = 0


def classify(
    diff: Diff,
    copies_right: dict[str, str],
    copies_left: dict[str, str],
) -> tuple[list[Finding], list[Finding], dict[str, int]]:
    """Turn a raw prolly diff into the two sides' findings.

    `copies_right` maps a digest that vanished from the left to a path
    where those bytes still live on the right; `copies_left` is the
    mirror. Both are empty when copy detection is off, which downgrades
    "gone but a copy remains" to a plain delete and "copy" to a plain
    add — never the other way round.
    """
    moves, residual_removed, residual_added = pair_moves(diff)

    move_links = [Link(m.src.path, m.dst.path, m.src.kind) for m in moves]
    roll_up(move_links)

    kept = [e for e in residual_removed if e.digest in copies_right]
    kept_links = [Link(e.path, copies_right[e.digest], e.kind) for e in kept]
    roll_up(kept_links)

    copied = [e for e in residual_added if e.digest in copies_left]
    copied_links = [Link(copies_left[e.digest], e.path, e.kind) for e in copied]
    roll_up(copied_links)

    gone = [e for e in residual_removed if e.digest not in copies_right]
    fresh = [e for e in residual_added if e.digest not in copies_left]

    left: list[Finding] = []
    right: list[Finding] = []

    for move, link in zip(moves, move_links):
        if link.covered:
            continue
        left.append(
            Finding(
                move.src,
                "moved_out",
                move.dst.path,
                f"moved to {move.dst.path}",
                link.rolled_up,
            )
        )
        right.append(
            Finding(
                move.dst,
                "moved_in",
                move.src.path,
                f"moved from {move.src.path}",
                link.rolled_up,
            )
        )

    for entry, link in zip(kept, kept_links):
        if link.covered:
            continue
        left.append(
            Finding(
                entry,
                "removed_but_copy_remains",
                link.dst,
                f"gone from here, identical bytes still at {link.dst}",
                link.rolled_up,
            )
        )

    for entry, link in zip(copied, copied_links):
        if link.covered:
            continue
        right.append(
            Finding(
                entry,
                "added_from_copy",
                link.src,
                f"new here, but identical bytes already existed at {link.src}",
                link.rolled_up,
            )
        )

    for entry in gone:
        left.append(
            Finding(
                entry,
                "removed",
                None,
                "directory gone — no directory on the right holds this exact subtree"
                if entry.kind == "dir"
                else "deleted — these bytes are nowhere on the right",
            )
        )

    for entry in fresh:
        right.append(
            Finding(
                entry,
                "added",
                None,
                "new directory — no directory on the left held this exact subtree"
                if entry.kind == "dir"
                else "new content — these bytes are nowhere on the left",
            )
        )

    for src, dst in diff.modified:
        if not src.path:
            continue
        if src.kind == "dir":
            # A directory's digest covers its children, so "modified"
            # here only ever means "something below me changed". That is
            # the tree structure doing its job, not a finding.
            left.append(Finding(src, "structure", src.path))
            right.append(Finding(dst, "structure", dst.path))
        else:
            left.append(Finding(src, "modified", dst.path, "content changed"))
            right.append(Finding(dst, "modified", src.path, "content changed"))

    counts = {
        "moves": sum(1 for link in move_links if not link.covered),
        "moved_entries": len(moves),
        "rolled_up": sum(
            link.rolled_up for link in move_links + kept_links + copied_links
        ),
        "removed": len(gone),
        "removed_but_copy_remains": sum(1 for link in kept_links if not link.covered),
        "added": len(fresh),
        "added_from_copy": sum(1 for link in copied_links if not link.covered),
        "modified": sum(
            1 for src, _ in diff.modified if src.kind != "dir" and src.path
        ),
    }
    return left, right, counts


# ---------------------------------------------------------------------
# building the two rendered trees
# ---------------------------------------------------------------------


def add_ancestors(paths: set[str]) -> set[str]:
    out = set(paths)
    for path in paths:
        parts = path.split("/")
        for i in range(1, len(parts)):
            out.add("/".join(parts[:i]))
    return out


def dup_annotations(groups: list[DupGroup]) -> dict[str, dict[str, object]]:
    """Per-path duplicate detail, for every member of every group."""
    out: dict[str, dict[str, object]] = {}
    for group in groups:
        for path in group.paths:
            out[path] = {
                "n": len(group.paths),
                "peers": [p for p in group.paths if p != path],
                "waste": group.wasted,
                "kind": group.kind,
                "size": group.size,
                "roll": group.rolled_up,
            }
    return out


def build_nodes(
    findings: list[Finding],
    full: dict[str, Entry] | None,
    dups: dict[str, dict[str, object]] | None = None,
) -> list[Node]:
    """Assemble one side's node list, keyed by path."""
    nodes: dict[str, Node] = {}
    for finding in findings:
        if not finding.entry.path:
            continue
        nodes[finding.entry.path] = Node(
            path=finding.entry.path,
            kind=finding.entry.kind,
            size=finding.entry.size,
            status=finding.status,
            peer=finding.peer,
            note=finding.note,
            rolled_up=finding.rolled_up,
        )

    if full is not None:
        for path, entry in full.items():
            if not path or path in nodes:
                continue
            nodes[path] = Node(
                path=path, kind=entry.kind, size=entry.size, status="unchanged"
            )

    for path, info in (dups or {}).items():
        node = nodes.get(path)
        if node is None:
            # A duplicate that the diff never mentioned — unchanged
            # between the two trees, but repeated inside this one. It
            # still has to appear, or the finding has nowhere to land.
            node = Node(
                path=path,
                kind=as_str(info.get("kind")) or "file",
                size=as_int(info.get("size")),
                status="unchanged",
            )
            nodes[path] = node
        node.dup = info

    for path in add_ancestors(set(nodes)):
        if path and path not in nodes:
            nodes[path] = Node(path=path, kind="dir", size=0, status="structure")

    return sorted(nodes.values(), key=lambda n: n.path)


# ---------------------------------------------------------------------
# the intermediate representation
# ---------------------------------------------------------------------
#
# Everything above this line either talks to a database or is a pure
# function over plain data. This section is the seam between the two:
# [`Inputs`] is everything that was *read*, [`DiffResult`] is everything
# that was *concluded*, and [`analyze`] is the pure function between
# them. The HTML is a projection of `DiffResult` and nothing more.
#
# The point of the seam is that the interesting behaviour — move
# pairing, subtree rollup, delete-vs-copy-remains, in-tree duplicates —
# is asserted against `DiffResult` in `dirtree_diff_test.py` with no
# doltlite and no browser anywhere near it.


@dataclass
class SideInput:
    """What was read from one side's database, before interpretation."""

    db: str
    ref: str
    commit: str
    # Every row at this commit. Populated only for `--full-tree`; None
    # means "render changed paths and their ancestors only".
    full: dict[str, Entry] | None = None
    # Rows at or above the duplicate threshold. Empty when the in-tree
    # duplicate scan is switched off.
    dup_candidates: list[Entry] = field(default_factory=list)


@dataclass
class Inputs:
    """Every byte read from both databases, and the flags that shaped it."""

    left: SideInput
    right: SideInput
    diff: Diff
    # digest -> a path on the right still holding those bytes
    copies_right: dict[str, str] = field(default_factory=dict)
    # digest -> a path on the left still holding those bytes
    copies_left: dict[str, str] = field(default_factory=dict)
    dup_threshold: int = 0
    copy_detection: bool = True
    unified: bool = False


@dataclass
class SideResult:
    """One side's conclusions."""

    db: str
    ref: str
    commit: str
    nodes: list[Node]
    dup_groups: list[DupGroup] = field(default_factory=list)

    @property
    def dup_wasted(self) -> int:
        return sum(g.wasted for g in self.dup_groups)


@dataclass
class DiffResult:
    """The whole comparison as plain data. No SQL, no HTML."""

    left: SideResult
    right: SideResult
    summary: dict[str, object]

    def node(self, side: str, path: str) -> Node | None:
        """The node at `path` on `side`, or None. For tests and probes."""
        for candidate in getattr(self, side).nodes:
            if candidate.path == path:
                return candidate
        return None

    def statuses(self, side: str) -> dict[str, str]:
        """path -> status for one side, ignoring structural filler."""
        return {
            n.path: n.status
            for n in getattr(self, side).nodes
            if n.status != "structure"
        }

    def to_payload(self) -> dict[str, object]:
        """The JSON the viewer consumes. A projection, not the truth."""
        return {
            "left": self._side_payload(self.left),
            "right": self._side_payload(self.right),
            "summary": self.summary,
        }

    @staticmethod
    def _side_payload(side: SideResult) -> dict[str, object]:
        return {
            "db": side.db,
            "ref": side.ref,
            "commit": side.commit,
            # The groups in full, not just a count — otherwise the JSON
            # is a lossy debug dump rather than the representation, and
            # `from_payload` could not rebuild what `analyze` produced.
            "dup_groups": [
                {
                    "digest": g.digest,
                    "kind": g.kind,
                    "size": g.size,
                    "paths": list(g.paths),
                    "rolled_up": g.rolled_up,
                }
                for g in side.dup_groups
            ],
            "dup_wasted": side.dup_wasted,
            "nodes": node_json(side.nodes),
        }

    @classmethod
    def from_payload(cls, payload: dict[str, object]) -> DiffResult:
        """Rebuild a result from its JSON. The inverse of
        [`to_payload`], so a run captured with `--json` can be replayed
        — into a test, a regression comparison, or some other viewer —
        without a database.
        """
        summary = payload["summary"]
        return cls(
            left=cls._side_from_payload(payload["left"]),
            right=cls._side_from_payload(payload["right"]),
            summary=dict(summary) if isinstance(summary, dict) else {},
        )

    @staticmethod
    def _side_from_payload(raw: object) -> SideResult:
        if not isinstance(raw, dict):
            raise TypeError(f"expected a side object, got {type(raw)}")
        groups_raw = raw.get("dup_groups") or []
        groups: list[DupGroup] = []
        if isinstance(groups_raw, list):
            for g in groups_raw:
                if not isinstance(g, dict):
                    continue
                paths = g.get("paths")
                groups.append(
                    DupGroup(
                        digest=as_str(g.get("digest")),
                        kind=as_str(g.get("kind")),
                        size=as_int(g.get("size")),
                        paths=[as_str(x) for x in paths]
                        if isinstance(paths, list)
                        else [],
                        rolled_up=as_int(g.get("rolled_up")),
                    )
                )
        nodes_raw = raw.get("nodes")
        nodes = (
            [node_from_json(n) for n in nodes_raw if isinstance(n, dict)]
            if isinstance(nodes_raw, list)
            else []
        )
        return SideResult(
            db=as_str(raw.get("db")),
            ref=as_str(raw.get("ref")),
            commit=as_str(raw.get("commit")),
            nodes=nodes,
            dup_groups=groups,
        )


def analyze(inputs: Inputs) -> DiffResult:
    """Turn everything that was read into everything that is concluded.

    Pure. Given the same `Inputs` it returns the same `DiffResult`, so
    every behaviour worth testing can be tested by constructing an
    `Inputs` literal.
    """
    left_findings, right_findings, counts = classify(
        inputs.diff, inputs.copies_right, inputs.copies_left
    )

    left_dups = group_duplicates(inputs.left.dup_candidates)
    right_dups = group_duplicates(inputs.right.dup_candidates)

    left_nodes = build_nodes(
        left_findings, inputs.left.full, dup_annotations(left_dups)
    )
    right_nodes = build_nodes(
        right_findings, inputs.right.full, dup_annotations(right_dups)
    )

    summary: dict[str, object] = {
        **counts,
        "full_tree": inputs.left.full is not None,
        "copy_detection": inputs.copy_detection,
        "dup_threshold": inputs.dup_threshold,
        "unified": inputs.unified,
    }
    return DiffResult(
        left=SideResult(
            db=inputs.left.db,
            ref=inputs.left.ref,
            commit=inputs.left.commit,
            nodes=left_nodes,
            dup_groups=left_dups,
        ),
        right=SideResult(
            db=inputs.right.db,
            ref=inputs.right.ref,
            commit=inputs.right.commit,
            nodes=right_nodes,
            dup_groups=right_dups,
        ),
        summary=summary,
    )


# ---------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------


def node_from_json(item: dict[str, object]) -> Node:
    """Inverse of one entry of [`node_json`]."""
    dup = item.get("dup")
    return Node(
        path=as_str(item["p"]),
        kind=as_str(item["k"]),
        size=as_int(item["s"]),
        status=as_str(item["st"]),
        peer=as_str(item["peer"]) if item.get("peer") else None,
        note=as_str(item.get("note")),
        rolled_up=as_int(item.get("roll")),
        dup=dict(dup) if isinstance(dup, dict) else None,
    )


def node_json(nodes: list[Node]) -> list[dict[str, object]]:
    out: list[dict[str, object]] = []
    for node in nodes:
        item: dict[str, object] = {
            "p": node.path,
            "k": node.kind,
            "s": node.size,
            "st": node.status,
        }
        if node.peer:
            item["peer"] = node.peer
        if node.note:
            item["note"] = node.note
        if node.rolled_up:
            item["roll"] = node.rolled_up
        if node.dup:
            item["dup"] = node.dup
        out.append(item)
    return out


def render_html(result: DiffResult, out_path: Path) -> None:
    """Project a [`DiffResult`] onto the viewer template."""
    payload = result.to_payload()
    template = (Path(__file__).parent / "viewer.html.tmpl").read_text()
    # `</script>` inside the embedded JSON would end the script element
    # early; escaping the slash keeps it a valid JSON string.
    blob = json.dumps(payload, separators=(",", ":")).replace("</", "<\\/")
    out_path.write_text(template.replace("__DIRTREE_DIFF_DATA__", blob))


# ---------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------


def parse_side(spec: str) -> tuple[str, str]:
    """Split `path/to.doltlite_db#ref` into its parts (`#ref` optional)."""
    if "#" in spec:
        db, ref = spec.rsplit("#", 1)
        return db, ref
    return spec, "HEAD"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="dirtree_diff",
        description=(
            "Diff two fsindex directory scans and write a single "
            "self-contained HTML page showing the two trees side by "
            "side, with moves detected via directory tree-hashes."
        ),
    )
    parser.add_argument(
        "--left",
        required=True,
        help="left scan as PATH[#REF]; REF is a branch, HEAD~N, or a commit hash",
    )
    parser.add_argument("--right", required=True, help="right scan as PATH[#REF]")
    parser.add_argument(
        "-o", "--out", default="dirtree_diff.html", help="output HTML path"
    )
    parser.add_argument(
        "--full-tree",
        action="store_true",
        help=(
            "render every entry, including unchanged ones. Costs a full "
            "scan of both corpora; without it the page holds only changed "
            "paths plus their ancestor directories, which is derived from "
            "the diff alone and stays O(changes)."
        ),
    )
    parser.add_argument(
        "--dup-threshold",
        type=parse_size,
        default="1M",
        metavar="BYTES",
        help=(
            "report content duplicated WITHIN each tree, for entries at "
            "or above this size (accepts 4096, 64K, 1M, 2G). A directory "
            "counts as one entry, so a folder copied inside the same tree "
            "is a single finding. Costs one full scan per side; pass 0 to "
            "turn it off. Default: 1M"
        ),
    )
    parser.add_argument(
        "--no-copy-detection",
        action="store_true",
        help=(
            "skip the corpus scan that distinguishes 'deleted outright' "
            "from 'deleted here, identical bytes still elsewhere'"
        ),
    )
    parser.add_argument(
        "--json",
        metavar="PATH",
        help=(
            "also write the intermediate representation as JSON — the same "
            "structure the page is rendered from, useful for diffing runs or "
            "driving something other than this viewer"
        ),
    )
    parser.add_argument("--doltlite", help="path to the doltlite shell")
    parser.add_argument(
        "--keep-scratch",
        action="store_true",
        help="keep the unified scratch database for inspection",
    )
    args = parser.parse_args(argv)

    dolt = Dolt(find_doltlite(args.doltlite))
    left_db, left_ref = parse_side(args.left)
    right_db, right_ref = parse_side(args.right)
    for db in (left_db, right_db):
        if not Path(db).exists():
            raise SystemExit(f"no such database: {db}")

    left_hash = resolve_ref(dolt, left_db, left_ref)
    right_hash = resolve_ref(dolt, right_db, right_ref)
    if left_hash == right_hash and Path(left_db).resolve() == Path(right_db).resolve():
        print(
            "the two sides resolve to the same commit — nothing to diff",
            file=sys.stderr,
        )

    workdir = Path(tempfile.mkdtemp(prefix="dirtree_diff."))
    try:
        same_file = Path(left_db).resolve() == Path(right_db).resolve()
        if same_file:
            # Both refs already live in one chunk store, which is the
            # cheap case: branches of a single scan file share prolly
            # structure outright.
            ctx = left_db
        else:
            ctx = unify(dolt, left_db, right_db, workdir)

        diff = fetch_diff(dolt, ctx, left_hash, right_hash)

        copies_right: dict[str, str] = {}
        copies_left: dict[str, str] = {}
        if not args.no_copy_detection:
            # Only digests the move pairing could not already account
            # for need the corpus scan.
            _, residual_removed, residual_added = pair_moves(diff)
            copies_right = lookup_digests(
                dolt, ctx, right_hash, {e.digest for e in residual_removed}, True
            )
            copies_left = lookup_digests(
                dolt, ctx, left_hash, {e.digest for e in residual_added}, True
            )

        threshold = args.dup_threshold
        inputs = Inputs(
            left=SideInput(
                db=str(Path(left_db).resolve()),
                ref=left_ref,
                commit=left_hash,
                full=load_side(dolt, ctx, left_hash) if args.full_tree else None,
                dup_candidates=duplicate_candidates(
                    dolt, ctx, left_hash, threshold, True
                ),
            ),
            right=SideInput(
                db=str(Path(right_db).resolve()),
                ref=right_ref,
                commit=right_hash,
                full=load_side(dolt, ctx, right_hash) if args.full_tree else None,
                dup_candidates=duplicate_candidates(
                    dolt, ctx, right_hash, threshold, True
                ),
            ),
            diff=diff,
            copies_right=copies_right,
            copies_left=copies_left,
            dup_threshold=threshold,
            copy_detection=not args.no_copy_detection,
            unified=not same_file,
        )

        # Everything from here is pure. `analyze` is the whole of the
        # interpretation; the HTML and the JSON are both projections.
        result = analyze(inputs)

        render_html(result, Path(args.out))
        if args.json:
            Path(args.json).write_text(
                json.dumps(result.to_payload(), indent=2, sort_keys=True)
            )

        summary = result.summary
        print(
            f"wrote {args.out} — "
            f"{summary['moves']} move(s) "
            f"(+{summary['rolled_up']} rolled up), "
            f"{summary['modified']} modified, "
            f"{summary['added']} added, "
            f"{summary['removed']} deleted, "
            f"{summary['removed_but_copy_remains']} deleted-with-copy-remaining; "
            f"duplicates within each tree: {len(result.left.dup_groups)} group(s) "
            f"left ({result.left.dup_wasted} B), "
            f"{len(result.right.dup_groups)} right ({result.right.dup_wasted} B)"
        )
    finally:
        if args.keep_scratch:
            print(f"scratch database kept at {workdir}", file=sys.stderr)
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
