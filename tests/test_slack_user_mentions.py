"""Unit tests for `<@U…>` user-mention resolution in grid rows + thread title.

Slack channel-join system messages (and many bot-posted messages) arrive with
text like ``<@U07ABC>`` instead of a resolved display name. We render the QMD
body through `to_commonmark`, which substitutes mentions from `user_labels`;
but the *thread title* (used for the QMD H1 header and the slug in the
filename) and the *grid row text* (search snippet) bypassed that path and
stored the raw token, so the UI surfaced gibberish like
``<@USW6LPSU9> has joined the channel``.
"""

from __future__ import annotations

from ingest.grid_rows import _slack_thread_title
from ingest.providers.slack.mrkdwn import resolve_user_mentions


def test_resolve_user_mentions_swaps_known_id_for_label() -> None:
    assert (
        resolve_user_mentions(
            "<@U_PICARD> has joined the channel", {"U_PICARD": "Jean-Luc Picard"}
        )
        == "@Jean-Luc Picard has joined the channel"
    )


def test_resolve_user_mentions_falls_back_to_id_when_unknown() -> None:
    # Same behavior as `to_commonmark` so the two stay consistent.
    assert resolve_user_mentions("hi <@U_UNKNOWN>", {}) == "hi @U_UNKNOWN"


def test_resolve_user_mentions_respects_inline_label() -> None:
    assert resolve_user_mentions("hi <@U_PICARD|jlp>", {}) == "hi @jlp"


def test_resolve_user_mentions_leaves_non_mention_text_alone() -> None:
    # We do NOT want this helper to touch other mrkdwn (bold, urls, etc.);
    # the QMD title is plain text, not a markdown context.
    assert resolve_user_mentions("*bold* <https://x|x>", {}) == "*bold* <https://x|x>"


def test_slack_thread_title_resolves_mentions_when_labels_given() -> None:
    title = _slack_thread_title(
        "<@U_PICARD> has joined the channel", {"U_PICARD": "Jean-Luc Picard"}
    )
    assert title == "@Jean-Luc Picard has joined the channel"


def test_slack_thread_title_without_labels_keeps_old_behavior() -> None:
    # Defaulting user_labels to None preserves the legacy signature for any
    # callers that haven't been updated yet.
    title = _slack_thread_title("Mr. Data, status report.")
    assert title == "Mr. Data, status report."
