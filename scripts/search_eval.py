#!/usr/bin/env python3
"""Scores the search box against a set of queries whose answers are known.

An eval set names target rows and, for each, queries a person might type to
find it. `run` asks every query of the search applet once per answer (each
tab, and the fused answer a search with no tab gets), records where the
target ranked and how long each answer took, and prints the scores; `compare`
says which queries moved between two runs; `show` prints what a query found,
for judging the rest of the list by eye.

The eval set is JSON: {"targets": [{"id", "uuid", "markdown_uuid", "note",
"queries": [{"q", "style"}]}]}. A row counts as the target when it is the
target or another row of the same document (same `markdown_uuid`), since a
search answers with the message it landed on. An eval set built from a real
library is that library's data: keep it beside the library, never here.

By default `run` starts its own `datalib-applet unified_document_index` on the
root, which is the applet the app runs, minus the gateway. `--http` asks a
running `datalib-http` instead, through its gateway, so the times are the
ones the app sees.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import statistics
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

TABS = ["fields", "words", "meaning"]
MODES = [*TABS, "default"]
DEFAULT_APPLET = "bazel-bin/datalib/backend/bin/datalib-applet"


class Applet:
    """A search applet this script started, asked directly on its port."""

    def __init__(self, binary: str, root: Path):
        self.secret = secrets.token_hex(16)
        env = dict(os.environ)
        env["DATALIB_DAG_DATA_ROOT"] = str(root)
        env["DATALIB_APPLET_SECRET"] = self.secret
        env.setdefault("RUST_LOG", "warn")
        self.proc = subprocess.Popen(
            [binary, "unified_document_index", "-p", "0"],
            env=env,
            stdout=subprocess.PIPE,
            text=True,
        )
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            if line.startswith("DATALIB_APPLET_PORT="):
                self.port = int(line.split("=", 1)[1])
                break
        else:
            raise SystemExit(f"{binary} exited without announcing a port")

    def get(self, path: str) -> Any:
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}{path}",
            headers={"x-datalib-applet-secret": self.secret},
        )
        with urllib.request.urlopen(req, timeout=300) as r:
            return json.load(r)

    def close(self) -> None:
        self.proc.terminate()
        self.proc.wait(timeout=30)


class Gateway:
    """A running `datalib-http`, asked through its applet gateway."""

    def __init__(self, base: str, root: Path):
        self.base = base.rstrip("/")
        self.token = (root / "system/api-token").read_text().strip()

    def get(self, path: str) -> Any:
        req = urllib.request.Request(
            f"{self.base}/applet/unified_document_index{path}",
            headers={"Authorization": f"Bearer {self.token}"},
        )
        with urllib.request.urlopen(req, timeout=300) as r:
            return json.load(r)

    def close(self) -> None:
        pass


def connect(args: argparse.Namespace, root: Path) -> Applet | Gateway:
    return Gateway(args.http, root) if args.http else Applet(args.applet, root)


def search(get: Callable[[str], Any], q: str, mode: str, depth: int) -> dict[str, Any]:
    params = {"q": q, "limit": str(depth)}
    if mode != "default":
        params["tab"] = mode
    started = time.perf_counter()
    try:
        answer = get("/search?" + urllib.parse.urlencode(params))
    except (OSError, ValueError) as e:  # an answer that failed is a result to record
        return {"error": f"{type(e).__name__}: {e}", "ms": ms_since(started)}
    took = ms_since(started)
    echo = answer.get("query_echo") or {}
    problems = [*answer.get("refused", []), *answer.get("errors", [])]
    if echo.get("qmd_error"):
        problems.append(f"qmd: {echo['qmd_error']}")
    if echo.get("qmd_index_missing"):
        problems.append("no qmd index")
    rows = [
        {
            "uuid": r["uuid"],
            "markdown_uuid": r.get("markdown_uuid"),
            "source_id": r.get("source_id"),
            "kind": r.get("kind"),
            "title": (r.get("conversation_name") or "")[:80],
            "sender": (r.get("sender") or "")[:40],
            "score": r.get("score"),
            "snippet": " ".join((r.get("snippet") or "").split())[:200],
            "link": r.get("link"),
        }
        for r in answer.get("rows", [])
    ]
    out: dict[str, Any] = {"ms": took, "total": answer.get("total", 0), "rows": rows}
    if problems:
        out["error"] = "; ".join(problems)
    return out


def ms_since(started: float) -> int:
    return round((time.perf_counter() - started) * 1000)


def rank_of(target: dict[str, Any], rows: list[dict[str, Any]]) -> int | None:
    for i, r in enumerate(rows, 1):
        if r["uuid"] == target["uuid"] or (
            target.get("markdown_uuid")
            and r["markdown_uuid"] == target["markdown_uuid"]
        ):
            return i
    return None


DOC = "datalib:doc/"
TEXT = ":~:text="
SECTION_RE = re.compile(r'data-section-uuid="([^"]+)"')
WORD_RE = re.compile(r"[a-z0-9]{4,}")


def section_of(row: dict[str, Any]) -> str:
    """The section a row's link opens at (`datalib:doc/<page>#<section>`),
    else the row itself, for an answer from a build without links."""
    link = row.get("link") or ""
    if link.startswith(DOC) and "#" in link:
        section = link.split("#", 1)[1].split(TEXT, 1)[0]
        return section or row.get("markdown_uuid") or row["uuid"]
    return row["uuid"]


class Pages:
    """Each target page's sections in order, and each section's text, read
    once from the applet's `/chat/<page>`."""

    def __init__(self, get: Callable[[str], Any]):
        self.get = get
        self.read: dict[str, tuple[list[str], dict[str, str]]] = {}

    def of(self, page: str) -> tuple[list[str], dict[str, str]]:
        if page not in self.read:
            try:
                body = self.get(f"/chat/{urllib.parse.quote(page)}").get("body", "")
            except (OSError, ValueError):
                body = ""
            marks = list(SECTION_RE.finditer(body))
            order = [m.group(1) for m in marks]
            text = {
                m.group(1): re.sub(
                    r"<[^>]+>", " ", body[m.end() : nxt.start() if nxt else len(body)]
                ).lower()
                for m, nxt in zip(marks, [*marks[1:], None])
            }
            self.read[page] = (order, text)
        return self.read[page]


def landing(
    target: dict[str, Any], row: dict[str, Any], pages: Pages
) -> dict[str, Any] | None:
    """Where a hit on the target's page landed, for a target that is a
    section of its page: on the target section or not, how many sections
    off, and whether the words shown came from the target section."""
    if target["uuid"] == target.get("markdown_uuid"):
        return None
    order, text = pages.of(target["markdown_uuid"])
    section = section_of(row)
    off = (
        abs(order.index(section) - order.index(target["uuid"]))
        if section in order and target["uuid"] in order
        else None
    )
    shown = set(WORD_RE.findall(row["snippet"].lower()))
    there = set(WORD_RE.findall(text.get(target["uuid"], "")))
    snippet = len(shown & there) / len(shown) >= 0.6 if len(shown) >= 3 else None
    return {"section": section == target["uuid"], "off": off, "snippet": snippet}


def ui_answer(by_tab: dict[str, dict[str, Any]]) -> dict[str, Any]:
    """What the app opens: the app asks every tab at once and opens the first
    to come back with rows, so the fastest tab that has any."""
    answered = [t for t in TABS if t in by_tab and by_tab[t].get("total")]
    if not answered:
        return {
            "tab": None,
            "rank": None,
            "ms": max((by_tab[t]["ms"] for t in by_tab), default=0),
        }
    tab = min(answered, key=lambda t: by_tab[t]["ms"])
    return {"tab": tab, "rank": by_tab[tab].get("rank"), "ms": by_tab[tab]["ms"]}


def run(args: argparse.Namespace) -> None:
    evals = json.loads(Path(args.evals).read_text())
    modes = args.modes.split(",")
    for m in modes:
        if m not in MODES:
            raise SystemExit(f"--modes: {m!r} is not one of {', '.join(MODES)}")
    root = Path(args.root).expanduser()
    server = connect(args, root)
    pages = Pages(server.get)
    results: list[dict[str, Any]] = []
    try:
        # The first answer of each kind pays for loading what it reads
        # (qmd's daemon, the embedding model); time the warm ones.
        for m in modes:
            search(server.get, "warm up", m, 1)
        only = set(args.only.split(",")) if args.only else None
        for t in evals["targets"]:
            if only and t["id"] not in only:
                continue
            for query in t["queries"]:
                by_mode = {}
                for m in modes:
                    a = search(server.get, query["q"], m, args.depth)
                    a["rank"] = rank_of(t, a.get("rows", []))
                    if a["rank"]:
                        a["landing"] = landing(t, a["rows"][a["rank"] - 1], pages)
                    by_mode[m] = a
                if all(tab in by_mode for tab in TABS):
                    by_mode["ui"] = ui_answer(by_mode)
                results.append(
                    {
                        "id": t["id"],
                        "source_id": t.get("source_id"),
                        "q": query["q"],
                        "style": query.get("style", ""),
                        "modes": by_mode,
                    }
                )
                line = "  ".join(
                    f"{m}={fmt_rank(by_mode[m].get('rank'))} ({by_mode[m]['ms']}ms)"
                    for m in by_mode
                )
                print(f"{t['id']} {query['q'][:60]:<60} {line}", file=sys.stderr)
    finally:
        server.close()
    record = {
        "root": str(root),
        "evals": str(args.evals),
        "depth": args.depth,
        "via": args.http or "applet",
        "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "results": results,
    }
    Path(args.out).write_text(json.dumps(record, indent=1, ensure_ascii=False) + "\n")
    print(report(record))


def fmt_rank(r: int | None) -> str:
    return "-" if r is None else str(r)


def scores(results: list[dict[str, Any]], mode: str) -> dict[str, float] | None:
    answers = [r["modes"][mode] for r in results if mode in r["modes"]]
    if not answers:
        return None
    ranks = [a.get("rank") for a in answers]
    n = len(answers)
    ms = sorted(a["ms"] for a in answers)

    def found(k: int) -> float:
        return sum(1 for r in ranks if r is not None and r <= k) / n

    return {
        "n": n,
        "@1": found(1),
        "@3": found(3),
        "@10": found(10),
        "any": found(10**9),
        "mrr": sum(1 / r for r in ranks if r) / n,
        "errors": sum(1 for a in answers if a.get("error")),
        "p50_ms": statistics.median(ms),
        "p90_ms": ms[min(n - 1, int(n * 0.9))],
        "max_ms": ms[-1],
    }


def report(record: dict[str, Any]) -> str:
    results = record["results"]
    modes = [m for m in [*MODES, "ui"] if any(m in r["modes"] for r in results)]
    lines = [
        (
            f"{len(results)} queries over {len({r['id'] for r in results})} targets, "
            f"top {record['depth']}, via {record['via']}"
        ),
        "",
        "| answer | n | @1 | @3 | @10 | found | MRR | errors | p50 ms | p90 ms | max ms |",
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for m in modes:
        s = scores(results, m)
        if s:
            lines.append(
                f"| {m} | {s['n']} | {s['@1']:.0%} | {s['@3']:.0%} | {s['@10']:.0%} | "
                f"{s['any']:.0%} | {s['mrr']:.2f} | {s['errors']} | {s['p50_ms']:.0f} | "
                f"{s['p90_ms']:.0f} | {s['max_ms']:.0f} |"
            )
    lines += [
        "",
        "where a hit on the page lands, for targets that are a section of a page:",
        "",
        "| answer | hits | on the section | within 2 | median off | snippet from it |",
        "|---|---|---|---|---|---|",
    ]
    for m in modes:
        landed = [
            r["modes"][m]["landing"]
            for r in results
            if r["modes"].get(m, {}).get("landing")
        ]
        if not landed:
            continue
        offs = [x["off"] for x in landed if x["off"] is not None]
        judged = [x["snippet"] for x in landed if x["snippet"] is not None]
        n = len(landed)
        snippet = f"{sum(judged) / len(judged):.0%} of {len(judged)}" if judged else "-"
        lines.append(
            f"| {m} | {n} | {sum(x['section'] for x in landed) / n:.0%} | "
            f"{sum(1 for o in offs if o <= 2) / n:.0%} | "
            f"{f'{statistics.median(offs):g}' if offs else '-'} | {snippet} |"
        )
    for split in ("style", "source_id"):
        lines += ["", f"found in the top 10, by {split}:", ""]
        lines.append("| " + split + " | n | " + " | ".join(modes) + " |")
        lines.append("|---" * (len(modes) + 2) + "|")
        for value in sorted({str(r.get(split)) for r in results}):
            subset = [r for r in results if str(r.get(split)) == value]
            cells = []
            for m in modes:
                s = scores(subset, m)
                cells.append("" if s is None else f"{s['@10']:.0%}")
            lines.append(f"| {value} | {len(subset)} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def compare(args: argparse.Namespace) -> None:
    before = json.loads(Path(args.before).read_text())
    after = json.loads(Path(args.after).read_text())
    old = {(r["id"], r["q"]): r for r in before["results"]}
    print("before:\n" + report(before) + "\n\nafter:\n" + report(after) + "\n")
    print("| target | query | answer | before | after |")
    print("|---|---|---|---|---|")
    for r in after["results"]:
        was = old.get((r["id"], r["q"]))
        if not was:
            continue
        for m, a in r["modes"].items():
            # Before the fused answer, the answer with no tab was qmd's hybrid.
            b = was["modes"].get(m) or (
                was["modes"].get("hybrid") if m == "default" else None
            )
            if b is None or b.get("rank") == a.get("rank"):
                continue
            print(
                f"| {r['id']} | {r['q']} | {m} | {fmt_rank(b.get('rank'))} | {fmt_rank(a.get('rank'))} |"
            )


def show(args: argparse.Namespace) -> None:
    record = json.loads(Path(args.results).read_text())
    for r in record["results"]:
        if args.id and r["id"] not in args.id.split(","):
            continue
        if args.missed and all(
            a.get("rank") is not None and a["rank"] <= 10
            for m, a in r["modes"].items()
            if m in args.missed.split(",")
        ):
            continue
        print(f"\n=== {r['id']} [{r['style']}] {r['q']}")
        for m, a in r["modes"].items():
            if m == "ui":
                print(f"  ui opens {a['tab']}: rank {fmt_rank(a['rank'])}")
                continue
            print(
                f"  {m}: rank {fmt_rank(a.get('rank'))} of {a.get('total')} in {a['ms']} ms"
                + (f"  ERROR {a['error']}" if a.get("error") else "")
            )
            for i, row in enumerate(a.get("rows", [])[: args.top], 1):
                print(
                    f"    {i:2} {row['source_id']:<22} {row['kind'] or '':<20} "
                    f"{row['title'][:40]:<40} | {row['snippet'][:90]}"
                )


def audit_me(args: argparse.Namespace) -> None:
    """Whether `me` is tagged where it should be. For each source: its
    rows, and how many `from:me`, `to:me` and `with:me` find. With
    `--name`, also the rows written under that name that `from:me` misses,
    and the rows `from:me` finds under another name, a few of each shown."""
    root = Path(args.root).expanduser()
    server = connect(args, root)

    def ask(q: str, limit: int = 0) -> dict[str, Any]:
        answer = search(server.get, q, "fields", limit)
        if answer.get("error"):
            raise SystemExit(f"{q!r}: {answer['error']}")
        return answer

    try:
        groups = server.get(
            "/search/groups?" + urllib.parse.urlencode({"q": "", "by": "source_ref"})
        )
        sources = sorted(g["values"][0] for g in groups["groups"] if g["values"][0])
        header = "| source | rows | from:me | to:me | with:me |"
        if args.name:
            header += f' from "{args.name}", not me | me, not "{args.name}" |'
        print(header)
        print("|---" * (header.count("|") - 1) + "|")
        shown: list[str] = []
        for source in sources:
            scope = f"source_id:{source}"
            cells = [str(ask(scope)["total"])]
            cells += [
                str(ask(f"{role}:me {scope}")["total"])
                for role in ("from", "to", "with")
            ]
            if args.name:
                for q in (
                    f'from:"{args.name}" -from:me {scope}',
                    f'from:me -from:"{args.name}" {scope}',
                ):
                    found = ask(q, args.samples)
                    cells.append(str(found["total"]))
                    for row in found["rows"]:
                        shown.append(
                            f"{q}\n    {row['sender']} | {row['title'][:50]} | {row['snippet'][:80]}"
                        )
            print(f"| {source} | " + " | ".join(cells) + " |")
        if shown:
            print("\nsamples:")
            print("\n".join(shown))
    finally:
        server.close()


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run", help="ask every query and score the answers")
    r.add_argument("--root", required=True, help="the data root to search")
    r.add_argument("--evals", required=True, help="the eval set (JSON)")
    r.add_argument("--out", required=True, help="where to write the results (JSON)")
    r.add_argument(
        "--modes",
        default=",".join(MODES),
        help="answers to ask for, of " + ",".join(MODES),
    )
    r.add_argument(
        "--depth", type=int, default=20, help="rows to read from each answer"
    )
    r.add_argument("--only", help="target ids to run, comma-separated")
    r.add_argument(
        "--applet", default=DEFAULT_APPLET, help="the datalib-applet binary to start"
    )
    r.add_argument(
        "--http", help="a running datalib-http's base URL, to ask through its gateway"
    )
    r.set_defaults(func=run)
    c = sub.add_parser("compare", help="what moved between two runs")
    c.add_argument("before")
    c.add_argument("after")
    c.set_defaults(func=compare)
    s = sub.add_parser("show", help="print what each query found")
    s.add_argument("results")
    s.add_argument("--id", help="target ids, comma-separated")
    s.add_argument(
        "--missed", help="only queries that missed the top 10 in any of these answers"
    )
    s.add_argument("--top", type=int, default=5)
    s.set_defaults(func=show)
    a = sub.add_parser(
        "audit-me",
        help="per source, the rows `me` finds in each role, and where it and the owner's name disagree",
    )
    a.add_argument("--root", required=True, help="the data root to search")
    a.add_argument(
        "--name", help="a name the owner writes under, to check `from:me` against"
    )
    a.add_argument(
        "--samples", type=int, default=3, help="rows to show of each disagreement"
    )
    a.add_argument(
        "--applet", default=DEFAULT_APPLET, help="the datalib-applet binary to start"
    )
    a.add_argument(
        "--http", help="a running datalib-http's base URL, to ask through its gateway"
    )
    a.set_defaults(func=audit_me)
    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
