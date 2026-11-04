#!/usr/bin/env python3
"""Scores the search box against a set of queries whose answers are known.

An eval set names target rows and, for each, queries a person might type to
find it. `run` asks every query of the search applet once per answer (each
tab, and the fused answer a search with no tab gets), records where the
target ranked and how long each answer took, and prints the scores; `compare`
says which queries moved between two runs; `show` prints what a query found,
for judging the rest of the list by eye. `--modes app` instead sends each
query the way the app does, every request at once, and times what a person
waits for: run it on its own, since each answer leaves the others cached.

The eval set is JSON: {"targets": [{"id", "uuid", "markdown_uuid", "note",
"queries": [{"q", "style"}]}]}. A row counts as the target when it is the
target or another row of the same document (same `markdown_uuid`), since a
search answers with the message it landed on. An optional "tz" (an IANA
zone) is the zone the applet runs in, so a bare date in a query
(`after:2369-04-15`) names the same day wherever the run is. An eval set
built from a real library is that library's data: keep it beside the
library, never here.

By default `run` starts its own `datalib-applet unified_document_index` on the
root, which is the applet the app runs, minus the gateway. `--http` asks a
running `datalib-http` instead, through its gateway, the path the app's
requests take. Use it for any time you mean to quote.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import platform
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
MODES = [*TABS, "fused"]
APP = "app"
BUDGET_MS = 1000
DEFAULT_APPLET = "bazel-bin/datalib/backend/bin/datalib-applet"
REPO = Path(__file__).resolve().parent.parent


class Applet:
    """A search applet this script started, asked directly on its port."""

    def __init__(self, binary: str, root: Path, tz: str | None = None):
        self.secret = secrets.token_hex(16)
        env = dict(os.environ)
        if tz:
            env["TZ"] = tz
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


def connect(
    args: argparse.Namespace, root: Path, tz: str | None = None
) -> Applet | Gateway:
    if args.http and tz:
        print(
            f"--http: the app reads dates in its own zone, not the eval set's {tz}",
            file=sys.stderr,
        )
    return Gateway(args.http, root) if args.http else Applet(args.applet, root, tz)


def search(get: Callable[[str], Any], q: str, mode: str, depth: int) -> dict[str, Any]:
    params = {"q": q, "limit": str(depth)}
    if mode != "fused":
        params["tab"] = mode
    return read_answer(get, "/search?" + urllib.parse.urlencode(params))


def read_answer(get: Callable[[str], Any], path: str) -> dict[str, Any]:
    started = time.perf_counter()
    try:
        answer = get(path)
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
SECTION_RE = re.compile(r'data-section-uuid="([^"]+)"')
WORD_RE = re.compile(r"[a-z0-9]{4,}")


def section_of(row: dict[str, Any]) -> str:
    """The section a row's link opens at (`datalib:doc/<page>#<section>`),
    else the row itself, for an answer from a build without links."""
    link = row.get("link") or ""
    if link.startswith(DOC):
        return link.partition("#")[2] or row.get("markdown_uuid") or row["uuid"]
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


# The list a search opens on asks for this many rows (`PAGE` in
# ui/src/cards/SearchList.ce.vue).
APP_PAGE = 50
# ui/src/grid/query.ts's `WORD` and `FILTER`, ui/src/cards/search.ts's `SOURCE`.
QUERY_WORD = re.compile(r'-?[A-Za-z_][\w.]*:"(?:[^"\\]|\\.)*"?|"(?:[^"\\]|\\.)*"?|\S+')
FILTER_WORD = re.compile(r"^-?[A-Za-z_][\w.]*:")
SOURCE_WORD = re.compile(r"^(source|source_id):")
CARRIES_TEXT = ("qmd:", "qmd_vsearch:")


def has_free_text(q: str) -> bool:
    return any(
        w.startswith(CARRIES_TEXT) or not FILTER_WORD.match(w)
        for w in QUERY_WORD.findall(q)
    )


def app_requests(q: str) -> dict[str, str]:
    """What the search card sends for a new search, by what each is for: the
    rows of the list it opens on (ui/src/cards/SearchList.ce.vue), one row of
    each tab for its count (`askTabs` in GridCard.ce.vue), and the groups
    behind the source chips, which ignore the query's own source
    (`loadSources`)."""

    def rows(limit: int, tab: str | None = None) -> str:
        params = {"q": q, "limit": str(limit)}
        if tab:
            params["tab"] = tab
        return "/search?" + urllib.parse.urlencode(params)

    others = [w for w in QUERY_WORD.findall(q) if not SOURCE_WORD.match(w)]
    chips = urllib.parse.urlencode({"q": " ".join(others), "by": "source_ref"})
    asked = {"rows": rows(APP_PAGE), "chips": "/search/groups?" + chips}
    if has_free_text(q):
        asked["count best"] = rows(1)
        asked |= {f"count {tab}": rows(1, tab) for tab in TABS}
    return asked


def ask_as_app(get: Callable[[str], Any], q: str) -> dict[str, Any]:
    """Every request at once, as the card sends them. `ms` is how long until
    the rows arrive, which is what a person waits for; `settled_ms` is how
    long until the tabs and the chips have answered too."""
    asked = app_requests(q)
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(asked)) as pool:
        running = {
            name: pool.submit(read_answer, get, path) for name, path in asked.items()
        }
        answers = {name: f.result() for name, f in running.items()}
    times = {name: a["ms"] for name, a in answers.items()}
    return {**answers["rows"], "settled_ms": max(times.values()), "requests": times}


def run(args: argparse.Namespace) -> None:
    evals = json.loads(Path(args.evals).read_text())
    modes = args.modes.split(",")
    for m in modes:
        if m not in [*MODES, APP]:
            raise SystemExit(f"--modes: {m!r} is not one of {', '.join(MODES)}, {APP}")
    if APP in modes and len(modes) > 1:
        raise SystemExit(
            f"--modes: ask {APP} on its own: an answer asked first leaves the next "
            "one's work cached, and its time would say nothing"
        )
    root = Path(args.root).expanduser()
    # Read before the first query: the checkout can move while a run takes
    # its half hour, and the applet was built from what it was at the start.
    made = provenance(args, evals.get("tz"))
    server = connect(args, root, evals.get("tz"))
    pages = Pages(server.get)
    results: list[dict[str, Any]] = []
    try:
        # The first answer of each kind pays for loading what it reads
        # (the applet itself, behind a gateway; the embedding model; the
        # vectors): kept apart, and the rest are timed warm.
        first_ms = {
            m: (
                ask_as_app(server.get, "warm up")
                if m == APP
                else search(server.get, "warm up", m, 1)
            )["ms"]
            for m in modes
        }
        held = corpus(server.get)
        if not held:
            raise SystemExit(
                f"{root} has no rows this build can read: sync it with this build first"
            )
        only = set(args.only.split(",")) if args.only else None
        for t in evals["targets"]:
            if only and t["id"] not in only:
                continue
            for query in t["queries"]:
                q = scoped(query["q"], t) if args.scope == "source" else query["q"]
                by_mode = {}
                for m in modes:
                    a = (
                        ask_as_app(server.get, q)
                        if m == APP
                        else search(server.get, q, m, args.depth)
                    )
                    a["rank"] = rank_of(t, a.get("rows", []))
                    if a["rank"]:
                        a["landing"] = landing(t, a["rows"][a["rank"] - 1], pages)
                    by_mode[m] = a
                results.append(
                    {
                        "id": t["id"],
                        "source_id": t.get("source_id"),
                        "q": query["q"],
                        "asked": q,
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
        "scope": args.scope,
        "corpus": held,
        "first_ms": first_ms,
        "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "provenance": {**made, "load": [*made["load"], round(os.getloadavg()[0], 1)]},
        "results": results,
    }
    Path(args.out).write_text(json.dumps(record, indent=1, ensure_ascii=False) + "\n")
    print(report(record))


def provenance(args: argparse.Namespace, tz: str | None) -> dict[str, Any]:
    """Where a run came from, so a kept run can be read beside another: the
    checkout the applet is meant to be built from, the machine and its load
    average as the run started and ended (times move with both), the zone
    its dates were read in, and which eval set."""

    def git(*a: str) -> str:
        return subprocess.run(
            ["git", *a], cwd=REPO, capture_output=True, text=True, check=True
        ).stdout.strip()

    return {
        "commit": git("rev-parse", "HEAD"),
        "dirty": git("status", "--porcelain", "--untracked-files=no") != "",
        "applet": None if args.http else args.applet,
        "host": platform.node(),
        "machine": f"{platform.system()} {platform.machine()}, {os.cpu_count()} cpus",
        "load": [round(os.getloadavg()[0], 1)],
        "tz": tz if tz and not args.http else time.strftime("%Z"),
        "evals_sha256": hashlib.sha256(Path(args.evals).read_bytes()).hexdigest(),
    }


def scoped(q: str, target: dict[str, Any]) -> str:
    """`q` narrowed to the target's source, as a person who knew where it was
    would ask; a query that names a source already is asked as it is."""
    return q if "source:" in q else f"{q} source:{target['source_id']}"


def corpus(get: Callable[[str], Any]) -> dict[str, int]:
    """How many rows each source holds: the haystack every query searches,
    so a score is read beside the corpus it was measured on."""
    groups = get(
        "/search/groups?" + urllib.parse.urlencode({"q": "", "by": "source_ref"})
    )
    return {g["values"][0] or "": g["count"] for g in groups["groups"]}


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
        "over_budget": sum(1 for x in ms if x > BUDGET_MS),
        "mrr": sum(1 / r for r in ranks if r) / n,
        "errors": sum(1 for a in answers if a.get("error")),
        "p50_ms": statistics.median(ms),
        "p90_ms": ms[min(n - 1, int(n * 0.9))],
        "max_ms": ms[-1],
    }


def corpus_line(held: dict[str, int]) -> str:
    return f"corpus: {sum(held.values())} rows in {len(held)} sources"


def report(record: dict[str, Any]) -> str:
    results = record["results"]
    modes = [m for m in [*MODES, APP, "ui"] if any(m in r["modes"] for r in results)]
    lines = [
        (
            f"{len(results)} queries over {len({r['id'] for r in results})} targets, "
            f"top {record['depth']}, via {record['via']}"
            + (", each narrowed to its target's source" if record.get("scope") else "")
        ),
        corpus_line(record.get("corpus") or {}),
        "",
        f"| answer | n | @1 | @3 | @10 | found | MRR | errors | p50 ms | p90 ms | max ms | over {BUDGET_MS} ms |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for m in modes:
        s = scores(results, m)
        if s:
            lines.append(
                f"| {m} | {s['n']} | {s['@1']:.0%} | {s['@3']:.0%} | {s['@10']:.0%} | "
                f"{s['any']:.0%} | {s['mrr']:.2f} | {s['errors']} | {s['p50_ms']:.0f} | "
                f"{s['p90_ms']:.0f} | {s['max_ms']:.0f} | {s['over_budget']} |"
            )
    lines += requests_table(results)
    if record.get("first_ms"):
        lines += ["", f"the first answer, cold, in ms: {record['first_ms']}"]
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


def requests_table(results: list[dict[str, Any]]) -> list[str]:
    """How long each request the app sends took, for a run of `app`."""
    taken: dict[str, list[int]] = {}
    for r in results:
        for name, ms in r["modes"].get(APP, {}).get("requests", {}).items():
            taken.setdefault(name, []).append(ms)
    settled = [
        r["modes"][APP]["settled_ms"]
        for r in results
        if "settled_ms" in r["modes"].get(APP, {})
    ]
    if not taken:
        return []
    taken["all of them answered"] = settled
    lines = [
        "",
        "what the app asks at once, and how long each took:",
        "",
        f"| request | n | p50 ms | p90 ms | max ms | over {BUDGET_MS} ms |",
        "|---|---|---|---|---|---|",
    ]
    for name, ms in taken.items():
        ms = sorted(ms)
        lines.append(
            f"| {name} | {len(ms)} | {statistics.median(ms):.0f} | "
            f"{ms[min(len(ms) - 1, int(len(ms) * 0.9))]} | {ms[-1]} | "
            f"{sum(1 for x in ms if x > BUDGET_MS)} |"
        )
    return lines


def load(path: str) -> dict[str, Any]:
    """A results file, its fused answer under that name: older files call it
    `default`, and before it existed qmd's hybrid stood in its place."""
    record = json.loads(Path(path).read_text())
    for r in record["results"]:
        modes = r["modes"]
        for old in ("default", "hybrid"):
            if old in modes and "fused" not in modes:
                modes["fused"] = modes.pop(old)
    return record


KEPT_ANSWER_KEYS = (
    "rank",
    "ms",
    "error",
    "landing",
    "tab",
    "total",
    "settled_ms",
    "requests",
)


def keep(args: argparse.Namespace) -> None:
    """A run, cut to what a history needs: its scores and each query's rank
    and time per answer, without the rows it found. `report` and `compare`
    read a kept run as they read a whole one; `show` needs the rows."""
    record = load(args.results)
    made = record.get("provenance") or {}
    commit = args.commit or made.get("commit")
    if not commit:
        raise SystemExit(
            f"{args.results} does not say what it was built from; pass --commit"
        )
    kept = {
        "note": args.note,
        "at": record["at"],
        "provenance": {**made, "commit": commit},
        **{k: record.get(k) for k in ("depth", "via", "scope", "corpus", "first_ms")},
        "results": [
            {
                **{k: r.get(k) for k in ("id", "source_id", "q", "asked", "style")},
                "modes": {
                    m: {k: a[k] for k in KEPT_ANSWER_KEYS if k in a}
                    for m, a in r["modes"].items()
                },
            }
            for r in record["results"]
        ],
    }
    day = record["at"][:10]
    slug = re.sub(r"[^a-z0-9]+", "-", args.note.lower()).strip("-")[:60].strip("-")
    out = Path(args.history) / f"{day}-{commit[:9]}-{slug}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(kept, indent=1, ensure_ascii=False) + "\n")
    print(out)


def ran_at(at: str) -> datetime.datetime:
    return datetime.datetime.strptime(at, "%Y-%m-%dT%H:%M:%S%z")


def history(args: argparse.Namespace) -> None:
    """Every kept run, oldest first: found in the top 10, mean reciprocal
    rank and median time per answer. Scores compare only between runs on
    the same eval set; a change of set is marked where it happens."""
    answers = [*MODES, APP]
    print("| at | commit | note | rows | load | " + " | ".join(answers) + " |")
    print("|---" * (len(answers) + 5) + "|")
    evals = None
    kept = [load(str(path)) for path in Path(args.history).glob("*.json")]
    for record in sorted(kept, key=lambda r: ran_at(r["at"])):
        made = record.get("provenance") or {}
        note = record.get("note", "")
        if evals is not None and made.get("evals_sha256") != evals:
            note = f"**new eval set** {note}"
        evals = made.get("evals_sha256")
        commit = made.get("commit", "?")[:9] + ("+" if made.get("dirty") else "")
        cells = []
        for m in answers:
            s = scores(record["results"], m)
            cells.append(
                ""
                if s is None
                else f"{s['@10']:.0%} · {s['mrr']:.2f} · {s['p50_ms']:.0f} ms"
            )
        rows = sum((record.get("corpus") or {}).values())
        busy = " to ".join(str(x) for x in made.get("load", [])) or "?"
        print(
            f"| {record['at'][:16]} | {commit} | {note} | {rows} | {busy} | "
            + " | ".join(cells)
            + " |"
        )


def compare(args: argparse.Namespace) -> None:
    before = load(args.before)
    after = load(args.after)
    old = {(r["id"], r["q"]): r for r in before["results"]}
    print("before:\n" + report(before) + "\n\nafter:\n" + report(after) + "\n")
    print("| target | query | answer | before | after |")
    print("|---|---|---|---|---|")
    for r in after["results"]:
        was = old.get((r["id"], r["q"]))
        if not was:
            continue
        for m, a in r["modes"].items():
            b = was["modes"].get(m)
            if b is None or b.get("rank") == a.get("rank"):
                continue
            print(
                f"| {r['id']} | {r['q']} | {m} | {fmt_rank(b.get('rank'))} | {fmt_rank(a.get('rank'))} |"
            )


def show(args: argparse.Namespace) -> None:
    record = load(args.results)
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
            scope = f"source:{source}"
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
        help=f"answers to ask for, of {','.join(MODES)}; or {APP} alone",
    )
    r.add_argument(
        "--depth", type=int, default=20, help="rows to read from each answer"
    )
    r.add_argument("--only", help="target ids to run, comma-separated")
    r.add_argument(
        "--scope",
        choices=["source"],
        help="narrow every query to its target's source, as `source:` would",
    )
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
    k = sub.add_parser("keep", help="add a run to a history of runs, cut to its scores")
    k.add_argument("results")
    k.add_argument("--history", required=True, help="the history's directory")
    k.add_argument("--note", required=True, help="what this run measured, a few words")
    k.add_argument(
        "--commit", help="the datalib commit, for a run that did not record one"
    )
    k.set_defaults(func=keep)
    h = sub.add_parser("history", help="the kept runs, oldest first")
    h.add_argument("history", help="the history's directory")
    h.set_defaults(func=history)
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
