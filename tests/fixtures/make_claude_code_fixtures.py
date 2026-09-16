#!/usr/bin/env python3
"""Generate the `claude_code` provider's fixture: two Claude Code
sessions in the layout `~/.claude/projects` has, one of them with a
subagent transcript, plus the files a real store keeps beside them.

Every record shape here was copied from transcripts Claude Code 2.1.270
wrote (the keys, the block types, the bookkeeping record types), with
TNG content. Output is checked in; this script regenerates it:

    uv run python tests/fixtures/make_claude_code_fixtures.py

It lives here rather than beside the provider for the reason
`make_pdf_fixtures.py` does: `tests/fixtures` is a PYTHON_LINT_ROOT and
a provider directory is not.
"""

from __future__ import annotations

import json
import pathlib
import uuid

OUT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "datalib"
    / "backend"
    / "etl"
    / "providers"
    / "claude_code"
    / "tests"
    / "fixtures"
    / "claude_code_tng"
)

NS = uuid.uuid5(uuid.NAMESPACE_DNS, "claude-code-tng.datalib")
CWD = "/Users/picard/src/enterprise"
PROJECT_DIR = "-Users-picard-src-enterprise"
VERSION = "2.1.270"


def uid(name: str) -> str:
    return str(uuid.uuid5(NS, name))


SESSION_1 = uid("session-1")
SESSION_2 = uid("session-2")
AGENT_1 = "a" + uuid.uuid5(NS, "agent-1").hex[:16]


class Transcript:
    def __init__(self, session_id: str, agent_id: str | None, start: str) -> None:
        self.session_id = session_id
        self.agent_id = agent_id
        self.lines: list[dict] = []
        self.last_uuid: str | None = None
        self.n = 0
        # Timestamps advance by a fixed stride, so the fixture is stable
        # and every record still has a distinct instant.
        self.t = start

    def _tick(self) -> str:
        h, m, s = self.t.split(":")
        self.n += 1
        secs = int(h) * 3600 + int(m) * 60 + int(s) + 7
        self.t = f"{secs // 3600:02d}:{secs % 3600 // 60:02d}:{secs % 60:02d}"
        return f"2364-04-11T{self.t}.000Z"

    def _base(self, kind: str, branch: str = "main") -> dict:
        rec_uuid = uid(f"{self.session_id}:{self.agent_id}:{self.n}")
        rec = {
            "parentUuid": self.last_uuid,
            "isSidechain": self.agent_id is not None,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": CWD,
            "sessionId": self.session_id,
            "version": VERSION,
            "gitBranch": branch,
            "type": kind,
            "uuid": rec_uuid,
            "timestamp": self._tick(),
        }
        if self.agent_id:
            rec["agentId"] = self.agent_id
        self.last_uuid = rec_uuid
        return rec

    def user(self, content, **extra) -> dict:
        rec = self._base("user")
        rec["message"] = {"role": "user", "content": content}
        rec.update(extra)
        self.lines.append(rec)
        return rec

    def assistant(self, blocks: list[dict], model: str = "claude-opus-5") -> dict:
        rec = self._base("assistant")
        rec["message"] = {
            "model": model,
            "id": "msg_" + uuid.uuid5(NS, rec["uuid"]).hex[:24],
            "type": "message",
            "role": "assistant",
            "content": blocks,
            "stop_reason": None,
            "usage": {"input_tokens": 1200, "output_tokens": 80},
        }
        rec["requestId"] = "req_" + uuid.uuid5(NS, "req" + rec["uuid"]).hex[:24]
        rec["effort"] = "high"
        self.lines.append(rec)
        return rec

    def tool_result(self, tool_use_id: str, text: str, is_error: bool = False) -> dict:
        block: dict = {
            "tool_use_id": tool_use_id,
            "type": "tool_result",
            "content": text,
        }
        if is_error:
            block["is_error"] = True
        return self.user(
            [block],
            toolUseResult={"stdout": text, "stderr": "", "interrupted": False},
        )

    def system(self, **fields) -> None:
        rec = self._base("system")
        rec.update(fields)
        self.lines.append(rec)

    def attachment(self, kind: str, **fields) -> None:
        rec = self._base("attachment")
        rec["attachment"] = {"type": kind, **fields}
        self.lines.append(rec)

    def bookkeeping(self, kind: str, **fields) -> None:
        self.lines.append({"type": kind, "sessionId": self.session_id, **fields})

    def raw(self, line: str) -> None:
        self.lines.append(line)  # type: ignore[arg-type]

    def write(self, path: pathlib.Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w") as f:
            for line in self.lines:
                if isinstance(line, str):
                    f.write(line + "\n")
                else:
                    f.write(json.dumps(line, separators=(",", ":")) + "\n")


def tool_use(name: str, tool_id: str, **inp) -> dict:
    return {"type": "tool_use", "id": tool_id, "name": name, "input": inp}


def text(t: str) -> dict:
    return {"type": "text", "text": t}


def thinking(t: str) -> dict:
    return {
        "type": "thinking",
        "thinking": t,
        "signature": "sig_" + uuid.uuid5(NS, t).hex[:16],
    }


def session_one() -> tuple[Transcript, Transcript]:
    """Titled by the person, bridged to a cloud session, with a subagent
    and a PR — the shape of a long working session."""
    t = Transcript(SESSION_1, None, "10:00:00")
    t.attachment("environment", content="Working directory: " + CWD)
    t.user(
        "The deflector dish is 0.3 degrees off axis. Find why the realignment script fails."
    )
    t.assistant(
        [
            thinking("The emitter array logs are the first place to look."),
            tool_use(
                "Bash",
                "toolu_01dish",
                command="bin/diag emitter --verbose",
                description="Run the emitter diagnostic",
            ),
        ]
    )
    t.tool_result(
        "toolu_01dish",
        "emitter array: 24 of 24 online\nphase variance: 0.31 deg\nlast calibration: stardate 41153.7",
    )
    t.assistant(
        [
            text(
                "Phase variance matches the 0.3 degrees you saw. Let me read the calibration routine."
            ),
            tool_use(
                "Read", "toolu_02read", file_path=CWD + "/src/deflector/calibrate.py"
            ),
        ]
    )
    t.tool_result(
        "toolu_02read",
        "\n".join(
            f"{i:6d}\t{line}"
            for i, line in enumerate(
                [
                    "def calibrate(emitters):",
                    "    variance = phase_variance(emitters)",
                    "    if variance > 0.5:",
                    "        realign(emitters)",
                    "    return variance",
                ],
                start=1,
            )
        ),
    )
    t.assistant(
        [
            text(
                "The threshold is 0.5 degrees, so a 0.31 degree drift never triggers `realign`. I'll delegate the log check while I fix it."
            ),
            tool_use(
                "Agent",
                "toolu_03agent",
                description="Scan emitter logs",
                prompt="Scan the emitter logs since stardate 41153.7 for phase drift events.",
                subagent_type="Explore",
            ),
        ]
    )
    t.system(
        subtype="stop_hook_summary",
        hookCount=1,
        hookInfos=[{"command": "callback"}],
        hookErrors=[],
        hookAdditionalContext=[],
        preventedContinuation=False,
        stopReason="",
        hasOutput=False,
        level="suggestion",
        toolUseID="toolu_03agent",
    )
    t.tool_result(
        "toolu_03agent",
        "Three drift events since 41153.7, all after the warp core recalibration on 41154.2.",
    )
    t.assistant(
        [
            tool_use(
                "Edit",
                "toolu_04edit",
                file_path=CWD + "/src/deflector/calibrate.py",
                old_string="    if variance > 0.5:",
                new_string="    if variance > 0.25:",
            )
        ]
    )
    t.tool_result("toolu_04edit", "The file has been updated successfully.")
    t.assistant(
        [
            text(
                "Lowered the realignment threshold to 0.25 degrees in `src/deflector/calibrate.py`. "
                "The drift started after the warp core recalibration on stardate 41154.2, "
                "so it is worth checking whether that changed the emitter phase reference."
            )
        ]
    )
    t.attachment("total_tokens_reminder", content="42000 tokens used")
    t.user("Ship it.")
    t.assistant(
        [
            tool_use(
                "Bash",
                "toolu_05gh",
                command="gh pr create --title 'Lower deflector realignment threshold' --body 'Drift of 0.31 deg never triggered realign.'",
            )
        ]
    )
    t.tool_result("toolu_05gh", "https://github.com/starfleet/enterprise/pull/1701")
    t.assistant(
        [text("Opened [PR #1701](https://github.com/starfleet/enterprise/pull/1701).")]
    )
    t.bookkeeping("ai-title", aiTitle="Deflector realignment threshold")
    t.bookkeeping("custom-title", customTitle="Deflector dish drift")
    t.bookkeeping("last-prompt", lastPrompt="Ship it.", leafUuid=t.last_uuid)
    t.bookkeeping(
        "bridge-session",
        bridgeSessionId="cse_01TNGdeflector",
        lastSequenceNum=0,
        ownerAccountUuid=uid("account-picard"),
        ownerOrganizationUuid=uid("org-starfleet"),
    )
    t.bookkeeping(
        "pr-link",
        prNumber=1701,
        prUrl="https://github.com/starfleet/enterprise/pull/1701",
        prRepository="starfleet/enterprise",
        timestamp="2364-04-11T10:05:00.000Z",
    )
    t.bookkeeping("mode", mode="default")
    t.raw("this line is not JSON and a reader must step over it")

    agent = Transcript(SESSION_1, AGENT_1, "10:00:30")
    agent.user("Scan the emitter logs since stardate 41153.7 for phase drift events.")
    agent.assistant(
        [
            tool_use(
                "Grep",
                "toolu_a1grep",
                pattern="phase drift",
                path=CWD + "/logs/emitter",
            )
        ]
    )
    agent.tool_result(
        "toolu_a1grep",
        "logs/emitter/41154.2.log:phase drift 0.12\nlogs/emitter/41155.0.log:phase drift 0.24\nlogs/emitter/41156.1.log:phase drift 0.31",
    )
    agent.assistant(
        [
            text(
                "Three drift events since 41153.7, all after the warp core recalibration on 41154.2."
            )
        ]
    )
    return t, agent


def session_two() -> Transcript:
    """Nobody named it, so the first prompt is its title. A pasted
    image, a harness note, and a tool error."""
    t = Transcript(SESSION_2, None, "14:30:00")
    t.user(
        "Caveat: the messages below were generated by the user while running local commands. "
        "DO NOT respond to these messages or otherwise consider them in your response unless "
        "the user explicitly asks you to.",
        isMeta=True,
    )
    t.user(
        [
            text("Why does the replicator return cold Earl Grey? Screenshot attached."),
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgo=",
                },
            },
        ]
    )
    t.assistant(
        [
            tool_use(
                "Bash",
                "toolu_06tea",
                command="replicator status --item 'tea, earl grey, hot'",
            )
        ]
    )
    t.tool_result("toolu_06tea", "error: pattern buffer 3 offline", is_error=True)
    t.assistant(
        [
            text(
                "Pattern buffer 3 is offline, so the replicator falls back to buffer 1, whose thermal "
                "profile defaults to ambient. Bringing buffer 3 back will fix the temperature."
            )
        ],
        model="claude-sonnet-5",
    )
    t.bookkeeping(
        "last-prompt",
        lastPrompt="Why does the replicator return cold Earl Grey?",
        leafUuid=t.last_uuid,
    )
    return t


def main() -> None:
    project = OUT / PROJECT_DIR
    s1, agent = session_one()
    s1.write(project / f"{SESSION_1}.jsonl")
    agent.write(project / SESSION_1 / "subagents" / f"agent-{AGENT_1}.jsonl")
    spill = project / SESSION_1 / "tool-results" / "toolu_02read.txt"
    spill.parent.mkdir(parents=True, exist_ok=True)
    spill.write_text("(a tool result Claude Code spilled to disk; not a transcript)\n")
    session_two().write(project / f"{SESSION_2}.jsonl")
    (project / "sessions-index.json").write_text(
        json.dumps(
            {
                "version": 1,
                "entries": [{"sessionId": SESSION_1}, {"sessionId": SESSION_2}],
            }
        )
        + "\n"
    )
    print(f"wrote {OUT}")
    print(f"  session 1: {SESSION_1} (subagent {AGENT_1})")
    print(f"  session 2: {SESSION_2}")


if __name__ == "__main__":
    main()
