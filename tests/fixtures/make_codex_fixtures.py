#!/usr/bin/env python3
"""Generate the `codex` provider's fixture: two Codex threads in the
layout `~/.codex` has, one of them the parent of a spawned sub-agent
thread, plus the `history.jsonl` a real home keeps beside them.

Every line shape here was copied from rollouts Codex 0.115 wrote (the
envelope, `session_meta`, `turn_context`, the `event_msg` types) or
read out of `codex-rs/protocol/src/models.rs` for the item types this
machine had not produced (`function_call`, `local_shell_call`,
`custom_tool_call` and their outputs, `reasoning`, `compacted`), with
TNG content. Output is checked in; this script regenerates it:

    uv run python tests/fixtures/make_codex_fixtures.py

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
    / "codex"
    / "tests"
    / "fixtures"
    / "codex_tng"
)

NS = uuid.uuid5(uuid.NAMESPACE_DNS, "codex-tng.datalib")
CWD = "/Users/picard/src/enterprise"
CLI_VERSION = "0.115.0"
MODEL = "gpt-5.3-codex"
GIT = {
    "commit_hash": "1701d0000000000000000000000000000000ncc",
    "branch": "main",
    "repository_url": "https://github.com/starfleet/enterprise.git",
}
BASE_INSTRUCTIONS = (
    "You are Codex, a coding agent based on GPT-5. You and the user share "
    "the same workspace and collaborate to achieve the user's goals."
)
AGENTS_MD = (
    "# Agent Instructions\n\nRun `diag` before touching any deflector "
    "setting, and never lower a threshold below 0.2."
)


def uid(name: str) -> str:
    """A stable UUIDv7-shaped id: Codex mints v7, whose leading bits are a
    timestamp; the fixture wants stability, not a real clock."""
    h = uuid.uuid5(NS, name).hex
    return f"{h[:8]}-{h[8:12]}-7{h[13:16]}-{h[16:20]}-{h[20:32]}"


THREAD_1 = uid("thread-1")
THREAD_2 = uid("thread-2")
AGENT_1 = uid("agent-1")


class Rollout:
    def __init__(self, thread_id: str, start: str, day: str) -> None:
        self.thread_id = thread_id
        self.day = day
        self.lines: list[dict | str] = []
        self.n = 0
        # Timestamps advance by a fixed stride, so the fixture is stable
        # and every line still has a distinct instant.
        self.t = start
        self.turn = 0

    def _tick(self) -> str:
        h, m, s = self.t.split(":")
        self.n += 1
        secs = int(h) * 3600 + int(m) * 60 + int(s) + 7
        self.t = f"{secs // 3600:02d}:{secs % 3600 // 60:02d}:{secs % 60:02d}"
        return f"2364-{self.day}T{self.t}.000Z"

    def line(self, kind: str, payload: dict) -> None:
        self.lines.append({"timestamp": self._tick(), "type": kind, "payload": payload})

    def session_meta(self, source: str | dict = "cli", **extra) -> None:
        p = {
            "id": self.thread_id,
            "timestamp": f"2364-{self.day}T{self.t}.000Z",
            "cwd": CWD,
            "originator": "codex_cli_rs",
            "cli_version": CLI_VERSION,
            "source": source,
            "model_provider": "openai",
            "base_instructions": {"text": BASE_INSTRUCTIONS},
            **extra,
            "git": GIT,
        }
        self.line("session_meta", p)

    def developer(self, text: str) -> None:
        self.item(
            {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": text}],
            }
        )

    def item(self, payload: dict) -> None:
        self.line("response_item", payload)

    def event(self, kind: str, **fields) -> None:
        self.line("event_msg", {"type": kind, **fields})

    def turn_start(self, prompt: str, model: str = MODEL, images: int = 0) -> str:
        self.turn += 1
        turn_id = uid(f"{self.thread_id}:turn:{self.turn}")
        self.event(
            "task_started",
            turn_id=turn_id,
            model_context_window=258400,
            collaboration_mode_kind="default",
        )
        self.line(
            "turn_context",
            {
                "turn_id": turn_id,
                "cwd": CWD,
                "current_date": f"2364-{self.day}",
                "timezone": "Earth/SanFrancisco",
                "approval_policy": "on-request",
                "sandbox_policy": {"type": "workspace-write", "network_access": False},
                "model": model,
                "personality": "pragmatic",
                "effort": "medium",
                "summary": "auto",
                "user_instructions": AGENTS_MD,
                "truncation_policy": {"mode": "tokens", "limit": 10000},
            },
        )
        content: list[dict] = [{"type": "input_text", "text": prompt}]
        content += [
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="}
        ] * images
        self.item({"type": "message", "role": "user", "content": content})
        self.event(
            "user_message", message=prompt, images=[], local_images=[], text_elements=[]
        )
        return turn_id

    def turn_end(self, turn_id: str, last: str | None = None) -> None:
        self.event("task_complete", turn_id=turn_id, last_agent_message=last)
        self.event(
            "token_count",
            info={
                "total_token_usage": {
                    "input_tokens": 9000,
                    "output_tokens": 300,
                    "total_tokens": 9300,
                }
            },
            rate_limits=None,
        )

    def reasoning(self, summary: str) -> None:
        self.item(
            {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": summary}],
                "content": None,
                "encrypted_content": "gAAAAAB" + uuid.uuid5(NS, summary).hex,
            }
        )
        self.event("agent_reasoning", text=summary)

    def _call_id(self) -> str:
        return "call_" + uuid.uuid5(NS, f"{self.thread_id}:{self.n}").hex[:24]

    def shell(self, cmd: str, output: str, exit_code: int = 0) -> None:
        call_id = self._call_id()
        argv = ["bash", "-lc", cmd]
        self.item(
            {
                "type": "function_call",
                "name": "shell",
                "arguments": json.dumps({"command": argv, "workdir": CWD}),
                "call_id": call_id,
            }
        )
        self.event("exec_command_begin", call_id=call_id, command=argv, cwd=CWD)
        self.item(
            {
                "type": "function_call_output",
                "call_id": call_id,
                "output": json.dumps(
                    {
                        "output": output,
                        "metadata": {"exit_code": exit_code, "duration_seconds": 0.4},
                    }
                ),
            }
        )
        self.event(
            "exec_command_end",
            call_id=call_id,
            exit_code=exit_code,
            stdout=output,
            stderr="",
        )

    def local_shell(self, argv: list[str], output: str) -> None:
        call_id = self._call_id()
        self.item(
            {
                "type": "local_shell_call",
                "call_id": call_id,
                "status": "completed",
                "action": {
                    "type": "exec",
                    "command": argv,
                    "timeout_ms": None,
                    "working_directory": CWD,
                    "env": None,
                    "user": None,
                },
            }
        )
        self.item(
            {"type": "function_call_output", "call_id": call_id, "output": output}
        )

    def apply_patch(self, patch: str, result: str) -> None:
        call_id = self._call_id()
        self.item(
            {
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call_id,
                "name": "apply_patch",
                "input": patch,
            }
        )
        self.item(
            {"type": "custom_tool_call_output", "call_id": call_id, "output": result}
        )

    def assistant(self, text: str, phase: str = "final_answer") -> None:
        self.item(
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
                "phase": phase,
            }
        )
        self.event("agent_message", message=text, phase=phase)

    def raw(self, line: str) -> None:
        self.lines.append(line)

    def write(self, path: pathlib.Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w") as f:
            for rec in self.lines:
                f.write(
                    rec if isinstance(rec, str) else json.dumps(rec, ensure_ascii=False)
                )
                f.write("\n")


def thread_1() -> Rollout:
    r = Rollout(THREAD_1, "10:00:00", "04-11")
    r.session_meta()
    r.developer(
        "<permissions instructions>\nFilesystem sandboxing: `workspace-write`. "
        "Network access is off."
    )
    r.item(
        {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": f"# AGENTS.md instructions for {CWD}\n\n<INSTRUCTIONS>\n{AGENTS_MD}\n</INSTRUCTIONS>",
                },
                {
                    "type": "input_text",
                    "text": f"<environment_context>\n  <cwd>{CWD}</cwd>\n  <shell>zsh</shell>\n</environment_context>",
                },
            ],
        }
    )
    r.developer(
        "<collaboration_mode># Collaboration Mode: Default\n\nYou are now in Default mode."
    )
    t = r.turn_start(
        "The deflector dish drifts 0.3 degrees every hour. Find out why and fix it."
    )
    r.reasoning(
        "**Checking the emitter alignment first**\n\nThe drift is periodic, so the "
        "phase variance log is the place to start."
    )
    r.shell(
        "diag emitter --phase",
        "phase variance: 0.31 deg\nemitter temp: 41C\nrealign threshold: 0.5 deg",
    )
    r.assistant(
        "The emitter reports 0.31 degrees of variance, under the 0.5 realignment "
        "threshold, so the dish never corrects itself.",
        phase="commentary",
    )
    r.apply_patch(
        "*** Begin Patch\n*** Update File: deflector/dish.conf\n@@\n"
        "-realign_threshold = 0.5\n+realign_threshold = 0.3\n*** End Patch",
        "Success. Updated the following files:\nM deflector/dish.conf",
    )
    r.shell(
        "diag emitter --phase",
        "phase variance: 0.02 deg\nemitter temp: 41C\nrealign threshold: 0.3 deg",
    )
    r.assistant(
        "Lowered the realignment threshold from 0.5 to 0.3 degrees in "
        "`deflector/dish.conf`; the dish now corrects at 0.31 and the variance is "
        "down to 0.02."
    )
    r.turn_end(t, "Lowered the realignment threshold…")
    # A second turn, interrupted.
    t = r.turn_start("Now do the same for the aft sensor array")
    r.reasoning("**Spawning a sub-agent to scan the aft logs**")
    r.item(
        {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": "<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>",
                }
            ],
        }
    )
    r.event("turn_aborted", turn_id=t, reason="interrupted")
    r.raw("this line is not JSON and must be stepped over, not fatal")
    return r


def agent_1() -> Rollout:
    r = Rollout(AGENT_1, "10:03:00", "04-11")
    r.session_meta(
        source={
            "subagent": {
                "thread_spawn": {
                    "parent_thread_id": THREAD_1,
                    "depth": 1,
                    "agent_nickname": "Data",
                    "agent_role": "explorer",
                }
            }
        },
        parent_thread_id=THREAD_1,
        agent_nickname="Data",
        agent_role="explorer",
    )
    t = r.turn_start("Scan the aft sensor logs for drift and report the worst hour.")
    r.local_shell(
        [
            "bash",
            "-lc",
            "grep drift /var/log/aft-sensors.log | sort -k3 -n | sed -n '$p'",
        ],
        "2364-04-11T03:00 drift 0.44 deg",
    )
    r.assistant("Worst hour: 03:00, at 0.44 degrees of drift.")
    r.turn_end(t)
    return r


def thread_2() -> Rollout:
    r = Rollout(THREAD_2, "14:30:00", "04-12")
    r.session_meta(source="exec")
    t = r.turn_start(
        "Why does the replicator return cold Earl Grey? Screenshot attached.",
        model="gpt-5.2-codex",
        images=1,
    )
    r.shell(
        "replicator status --beverage 'tea, earl grey, hot'",
        "error: pattern buffer 7 offline",
        exit_code=1,
    )
    r.line(
        "compacted",
        {
            "message": "Earlier in this thread: the replicator's pattern buffer 7 was found offline.",
            "replacement_history": None,
        },
    )
    r.assistant(
        "Pattern buffer 7 is offline, so the replicator falls back to the cold "
        "profile. Bring it back with `replicator buffer 7 --online`."
    )
    r.turn_end(t)
    return r


def main() -> None:
    day1 = OUT / "sessions" / "2364" / "04" / "11"
    thread_1().write(day1 / f"rollout-2364-04-11T10-00-00-{THREAD_1}.jsonl")
    agent_1().write(day1 / f"rollout-2364-04-11T10-03-00-{AGENT_1}.jsonl")
    thread_2().write(
        OUT
        / "archived_sessions"
        / "2364"
        / "04"
        / "12"
        / f"rollout-2364-04-12T14-30-00-{THREAD_2}.jsonl"
    )
    # The prompt index a real home keeps; not a rollout, and not read.
    with (OUT / "history.jsonl").open("w") as f:
        for tid, ts, text in [
            (
                THREAD_1,
                12441200400,
                "The deflector dish drifts 0.3 degrees every hour. Find out why and fix it.",
            ),
            (
                THREAD_2,
                12441303000,
                "Why does the replicator return cold Earl Grey? Screenshot attached.",
            ),
        ]:
            f.write(json.dumps({"session_id": tid, "ts": ts, "text": text}) + "\n")
    print(f"wrote {OUT}")
    print(f"THREAD_1={THREAD_1}\nTHREAD_2={THREAD_2}\nAGENT_1={AGENT_1}")


if __name__ == "__main__":
    main()
