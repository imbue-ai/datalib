"""Provider-specific downloaders.

Each module exposes a `fetch(args)` function and a `main(argv)` argparse
entry point. The contract: write provider-shaped JSON into a user-chosen
directory; *do not* touch Dolt, render, or anything else under our control.
The ingest pipeline (`ingest/`) is what turns those raw dirs into the
canonical mirror.

Two providers today: `claude_web` (claude.ai) and `chatgpt_web`
(chatgpt.com). Both behave the same way at the surface: incremental,
missing conversations fetched first (so a 429 spends our budget on
fetches that move us forward), tqdm progress.

Some sources don't go through this package at all — Anthropic's bulk
"takeout" export is dropped on disk by Anthropic and fed straight to
ingest. The downloaders are for the cases where there is no takeout.

Usage:
    uv run python -m download.claude_web   [options]
    uv run python -m download.chatgpt_web  [options]
"""
