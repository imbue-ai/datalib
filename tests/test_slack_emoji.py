"""Unit tests for `:shortcode:` → Unicode emoji conversion.

Slack messages and reactions arrive with shortcodes like `:wink:` or
`:robot_face:`. Without conversion they pass straight through to the
rendered QMD as literal `:name:` strings, which is what the user saw in
their feedback ("the :wink: should be an emoji").
"""

from __future__ import annotations

from ingest.providers.slack.mrkdwn import emojize_shortcodes, to_commonmark


def test_emojize_known_alias() -> None:
    assert emojize_shortcodes("hi :wink:") == "hi 😉"


def test_emojize_slack_specific_aliases() -> None:
    # `:robot_face:` and `:thumbsup:` are Slack-only spellings; emoji
    # library's alias mode covers them.
    assert emojize_shortcodes(":robot_face:") == "🤖"
    assert emojize_shortcodes(":thumbsup:") == "👍"
    assert emojize_shortcodes(":+1:") == "👍"


def test_emojize_unknown_shortcode_passes_through() -> None:
    # Custom Slack emoji (org-uploaded) won't be in the alias table; better
    # to leave the literal `:name:` than to drop it.
    assert emojize_shortcodes("ship it :rocketcat:") == "ship it :rocketcat:"


def test_to_commonmark_emojizes() -> None:
    # End-to-end: emoji conversion runs inside the standard pipeline so
    # message bodies render with real glyphs.
    assert to_commonmark(":wink:") == "😉"
