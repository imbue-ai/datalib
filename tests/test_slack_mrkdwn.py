"""Unit tests for the Slack mrkdwn → CommonMark fixup."""

from __future__ import annotations

from ingest.providers.slack.mrkdwn import to_commonmark


def test_bold_single_to_double_asterisk() -> None:
    assert to_commonmark("hello *world* friend") == "hello **world** friend"


def test_bold_multi_word() -> None:
    assert to_commonmark("*set phasers to stun*") == "**set phasers to stun**"


def test_bold_does_not_eat_isolated_asterisk() -> None:
    # Trailing asterisk not at a word boundary in pair form should be left alone.
    assert to_commonmark("a*b") == "a*b"
    # Already-doubled CommonMark bold should pass through unchanged.
    assert to_commonmark("**already bold**") == "**already bold**"


def test_strike_single_to_double_tilde() -> None:
    assert to_commonmark("oh ~hi~ there") == "oh ~~hi~~ there"


def test_url_with_label() -> None:
    assert (
        to_commonmark("see <https://imbue.com|Imbue> docs")
        == "see [Imbue](https://imbue.com) docs"
    )


def test_url_without_label() -> None:
    assert to_commonmark("see <https://imbue.com>") == "see <https://imbue.com>"


def test_mailto_with_label() -> None:
    assert (
        to_commonmark("mail <mailto:a@b.com|Alice>") == "mail [Alice](mailto:a@b.com)"
    )


def test_user_mention_resolves_via_lookup() -> None:
    assert (
        to_commonmark("hi <@U_PICARD>!", {"U_PICARD": "Jean-Luc Picard"})
        == "hi @Jean-Luc Picard!"
    )


def test_user_mention_falls_back_to_id() -> None:
    assert to_commonmark("hi <@U_PICARD>!") == "hi @U_PICARD!"


def test_user_mention_with_inline_label() -> None:
    # Slack sometimes inlines a label after `|` (older messages).
    assert to_commonmark("hi <@U_PICARD|picard>") == "hi @picard"


def test_channel_mention_uses_inline_name() -> None:
    assert to_commonmark("see <#C_BRIDGE|bridge>") == "see #bridge"


def test_special_mention_here() -> None:
    assert to_commonmark("<!here> meeting now") == "@here meeting now"


def test_subteam_mention() -> None:
    assert to_commonmark("<!subteam^S123|engineers> ping") == "@engineers ping"


def test_html_entities_decoded() -> None:
    assert to_commonmark("if a &lt; b &amp;&amp; b &gt; c") == "if a < b && b > c"


def test_blockquote_preserved_at_line_start() -> None:
    src = "From Sculptor:\n> Started in background.\n> Will notify."
    assert to_commonmark(src) == src


def test_blockquote_does_not_bleed_into_next_line() -> None:
    # Slack quotes only the `>` line itself; CommonMark would lazily extend
    # the blockquote to the next non-blank line. Inject a blank line so the
    # following text renders unquoted.
    src = "> accumulating complexity?\nyeah, that's what I meant."
    expected = "> accumulating complexity?\n\nyeah, that's what I meant."
    assert to_commonmark(src) == expected


def test_blockquote_group_stays_together() -> None:
    # Consecutive `>` lines form one Slack quote; we only inject a blank
    # line *after* the last `>` line of the group.
    src = "> line one\n> line two\nback to normal"
    expected = "> line one\n> line two\n\nback to normal"
    assert to_commonmark(src) == expected


def test_combined_message() -> None:
    src = (
        "TIL: you can `tail` *any* tqdm output, see "
        "<https://github.com/tqdm/tqdm|tqdm docs>.\n"
        "From <@U_PICARD>:\n"
        "> Started in background. Will notify when it finishes."
    )
    expected = (
        "TIL: you can `tail` **any** tqdm output, see "
        "[tqdm docs](https://github.com/tqdm/tqdm).\n"
        "From @Jean-Luc Picard:\n"
        "> Started in background. Will notify when it finishes."
    )
    assert to_commonmark(src, {"U_PICARD": "Jean-Luc Picard"}) == expected
