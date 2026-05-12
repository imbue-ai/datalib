"""Integration tests against the checked-in TNG-themed fake data.

These tests exercise the parser layer (no Dolt, no rendering) against the
fixtures under tests/fixtures/. They guard against silent breakage of the
fixtures when schemas evolve, and give a concrete demo of the row shapes
expected downstream.
"""

from __future__ import annotations

from pathlib import Path

from ingest.providers.anthropic.parse import parse_export
from ingest.providers.openai.parse import parse_api_dir
from ingest.providers.slack.parse import parse_api_dir as parse_slack_api_dir

FIXTURES = Path(__file__).parent / "fixtures"


def test_anthropic_export_fixture_parses() -> None:
    parsed = parse_export(FIXTURES / "anthropic_export")

    assert {a.full_name for a in parsed.accounts} == {
        "Jean-Luc Picard",
        "Geordi La Forge",
    }

    assert len(parsed.projects) == 1
    proj = parsed.projects[0]
    assert proj.name == "Holodeck Program Library"
    assert proj.account_uuid == "00000001-1701-4d00-8000-000000000001"

    convs_by_name = {c.name: c for c in parsed.conversations}
    assert set(convs_by_name) == {
        "Tea, Earl Grey, Hot",
        "Warp Plasma Conduit Resonance",
    }

    tea = convs_by_name["Tea, Earl Grey, Hot"]
    assert tea.project_uuid == "00000010-1701-4d00-8000-000000000010"

    warp = convs_by_name["Warp Plasma Conduit Resonance"]
    assert warp.project_uuid is None

    # The CSV attachment on the warp conversation must round-trip.
    warp_atch = [a for a in parsed.attachments if a.kind == "attachment"]
    assert any(
        "conduit-17-telemetry" in (a.raw_json.get("file_name") or "") for a in warp_atch
    )

    # Threading: every non-root message has a parent that is also a message.
    msg_ids = {m.message_uuid for m in parsed.messages}
    for m in parsed.messages:
        if m.parent_message_uuid is not None:
            assert m.parent_message_uuid in msg_ids


def test_anthropic_api_fixture_has_rich_block_types() -> None:
    parsed = parse_export(FIXTURES / "anthropic_api")

    block_types = {b.type for b in parsed.content_blocks}
    # The API-shape fixture is the one expected to exercise non-text blocks.
    assert {"text", "thinking", "tool_use", "tool_result"}.issubset(block_types)

    # The image-file-only message produces a "file" attachment, not "attachment".
    kinds = {a.kind for a in parsed.attachments}
    assert "file" in kinds


def test_chatgpt_api_fixture_parses() -> None:
    parsed = parse_api_dir(FIXTURES / "chatgpt_api")

    assert len(parsed.accounts) == 1
    assert parsed.accounts[0].name == "Lt. Cmdr. Data"

    titles = {c.title for c in parsed.conversations}
    assert "Sonnet on a Cat Named Spot" in titles
    assert "Polynomial Fit for Sensor Calibration" in titles
    # Third conversation deliberately exercises the failure mode where
    # ChatGPT's auto-titler leaves the full first user message as the
    # title — guards the conversation_name truncation path in ingest.
    long_titles = [t for t in titles if t and t.startswith("I have been reviewing")]
    assert len(long_titles) == 1 and len(long_titles[0]) > 512

    # The system message in the sonnet thread is content_type model_editable_context.
    sonnet_msgs = [
        m
        for m in parsed.messages
        if m.conversation_id == "68fa0001-fake-7000-8000-positronic0001"
    ]
    assert any(m.content_type == "model_editable_context" for m in sonnet_msgs)
    # Roles include system, user, assistant.
    assert {m.role for m in sonnet_msgs} >= {"system", "user", "assistant"}

    # The polyfit thread surfaces a code part with language=python.
    code_parts = [p for p in parsed.content_parts if p.kind == "code"]
    assert any(p.language == "python" for p in code_parts)

    # ChatGPT timestamps are normalized to ISO-8601 strings.
    for c in parsed.conversations:
        assert c.create_time is None or "T" in c.create_time


def test_slack_api_fixture_parses_with_unicode_line_separator() -> None:
    """Slack messages containing U+2028 (LINE SEPARATOR) in `raw.text` must
    not shred record boundaries.

    `slack_web.py` writes JSONL with `ensure_ascii=False`, which leaves
    U+2028 / U+2029 unescaped. `str.splitlines()` treats those as line
    breaks — so a naïve `path.read_text().splitlines()` loader will split
    a single record into pieces that no longer parse. The fixture's
    Data's-log message embeds a literal U+2028 to guard against
    regression of this exact failure mode.
    """
    parsed = parse_slack_api_dir(FIXTURES / "slack_api")
    log_msgs = [m for m in parsed.messages if "Stardate 47988.1" in (m.text or "")]
    assert len(log_msgs) == 1
    assert "\u2028" in log_msgs[0].text


def test_slack_api_fixture_dedupes_duplicated_message_records() -> None:
    """The fixture intentionally exercises two duplication sources:

    1. A thread root appears in both `message/created` (channel history)
       *and* `reply/created` — Slack's `conversations.replies` returns
       the parent message alongside the replies.
    2. A non-root message appears twice in `message/created` — simulates
       overlapping pages from a paginated history rescan.

    The parser must emit one MessageRow per uuid in both cases, or the
    row collides with itself in `grid_rows` (PRIMARY KEY uuid).
    """
    parsed = parse_slack_api_dir(FIXTURES / "slack_api")
    uuids = [m.uuid for m in parsed.messages]
    assert len(uuids) == len(set(uuids))

    # Sanity-check the two scenarios are actually present in the fixture
    # files — if they get removed by accident, the dedup test above goes
    # silently green without exercising anything.
    import json

    base = FIXTURES / "slack_api"
    message_lines = (
        (base / "message" / "created" / "events.jsonl").read_bytes().splitlines()
    )
    reply_lines = (
        (base / "reply" / "created" / "events.jsonl").read_bytes().splitlines()
    )
    msg_ts_counts: dict[tuple[str, str], int] = {}
    for line in message_lines:
        if not line.strip():
            continue
        ev = json.loads(line)
        key = (ev["channel_id"], ev["message_ts"])
        msg_ts_counts[key] = msg_ts_counts.get(key, 0) + 1
    assert any(c >= 2 for c in msg_ts_counts.values()), (
        "fixture should contain an intra-stream duplicate"
    )

    msg_keys = set(msg_ts_counts)
    reply_keys = {
        (json.loads(line)["channel_id"], json.loads(line)["raw"]["ts"])
        for line in reply_lines
        if line.strip()
    }
    assert msg_keys & reply_keys, (
        "fixture should contain a ts present in both message/ and reply/ streams"
    )
