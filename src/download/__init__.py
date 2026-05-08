"""Provider-specific downloaders.

Each module exposes a `fetch(...)` function and a `main()` Typer entry
point. The contract: write provider-shaped JSON or JSONL into a
user-chosen directory; *do not* touch Dolt, render, or anything else
under our control. The ingest pipeline (`ingest/`) is what turns those
raw dirs into the canonical mirror.

Three providers today:
- `claude_web`   (claude.ai web API → flat conversations.json shaped like
  Anthropic's bulk export)
- `chatgpt_web`  (chatgpt.com web API → one JSON file per conversation
  plus an index)
- `slack_web`    (slack.com web API → per-entity created/+updated/ JSONL
  event streams)

The two LLM-chat downloaders share a behavior: incremental, missing
conversations fetched first (so a 429 spends our budget on fetches that
move us forward), tqdm progress. Slack uses a different shape because
its data model is intrinsically multi-entity (channels, messages,
threads, reactions) rather than a single conversation tree.

Some sources don't go through this package at all — Anthropic's bulk
"takeout" export is dropped on disk by Anthropic and fed straight to
ingest. The downloaders are for the cases where there is no takeout.

Usage:
    uv run python -m download.claude_web   [options]
    uv run python -m download.chatgpt_web  [options]
    uv run python -m download.slack_web    [options]
"""
