"""Unit tests for Slack permalink construction.

Slack has two flavors of message URL:

- Channel-view: `/archives/{ch}/p{ts}?team={team}` — opens the channel
  scrolled to the message.
- Thread-pane: same path plus `&thread_ts={root_ts}&cid={ch}` — opens
  the side-pane with the reply selected.

For replies inside a thread we want the thread-pane variant; otherwise
clicking "view in Slack" lands on the channel and never reveals which
reply we meant.
"""

from __future__ import annotations

from ingest.grid_rows import _slack_link
from ingest.render import _slack_message_link


def test_root_message_uses_plain_permalink() -> None:
    url = _slack_message_link("T_NCC1701D", "C_BRIDGE", "12604000100.000100")
    assert (
        url
        == "https://slack.com/archives/C_BRIDGE/p12604000100000100?team=T_NCC1701D"
    )


def test_thread_reply_includes_thread_context_in_render() -> None:
    url = _slack_message_link(
        "T_NCC1701D",
        "C_BRIDGE",
        "12604000200.000200",
        thread_ts="12604000100.000100",
    )
    assert (
        url
        == "https://slack.com/archives/C_BRIDGE/p12604000200000200"
        "?team=T_NCC1701D&thread_ts=12604000100.000100&cid=C_BRIDGE"
    )


def test_root_message_when_ts_equals_thread_ts_stays_plain() -> None:
    # Passing thread_ts equal to ts (i.e. this *is* the root) should not
    # emit thread params — that would open the pane on itself.
    url = _slack_message_link(
        "T_NCC1701D",
        "C_BRIDGE",
        "12604000100.000100",
        thread_ts="12604000100.000100",
    )
    assert (
        url
        == "https://slack.com/archives/C_BRIDGE/p12604000100000100?team=T_NCC1701D"
    )


def test_grid_rows_link_matches_render_link() -> None:
    # The two helpers exist in different modules but must agree byte-for-byte,
    # otherwise grid rows and QMD frontmatter would link to different URLs
    # for the same message.
    args = ("T_NCC1701D", "C_BRIDGE", "12604000200.000200")
    assert _slack_link(*args, thread_ts="12604000100.000100") == _slack_message_link(
        *args, thread_ts="12604000100.000100"
    )
    assert _slack_link(*args) == _slack_message_link(*args)
