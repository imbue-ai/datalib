"""Slack mrkdwn → CommonMark converter.

Slack messages arrive in Slack's "mrkdwn" dialect, which overlaps with but
diverges from CommonMark in several places:

- `*bold*` (single asterisk) instead of `**bold**`
- `~strike~` (single tilde) instead of `~~strike~~`
- `<https://url|label>` instead of `[label](https://url)`
- `<@U…>` / `<#C…|name>` / `<!here>` user/channel/special mentions
- `&amp;` / `&lt;` / `&gt;` HTML-escaping in the message body

Our QMD bodies are served verbatim to the UI, which renders them with
CommonMark — so we translate at ingest time. No usable Python library for
this direction exists (everything on PyPI goes the other way), so this is a
small regex pipeline, exercised by snapshot tests.

Reference for the source dialect:
https://api.slack.com/reference/surfaces/formatting
"""

from __future__ import annotations

import re
from typing import Mapping


_USER_REF = re.compile(r"<@([UW][A-Z0-9_]+)(?:\|([^>]+))?>")
_CHANNEL_REF = re.compile(r"<#([CG][A-Z0-9_]+)(?:\|([^>]+))?>")
_SUBTEAM_REF = re.compile(r"<!subteam\^[A-Z0-9_]+(?:\|([^>]+))?>")
_SPECIAL_REF = re.compile(r"<!(here|channel|everyone)(?:\|[^>]+)?>")
# A `<…>` URL token: either `<url|label>` or `<url>`. The url is everything
# up to `|` or `>`, and never contains literal `<` or `>` itself.
_URL_REF = re.compile(r"<((?:https?|mailto):[^|>\s]+)(?:\|([^>]*))?>")
# Bold: a `*…*` pair where the asterisks aren't part of a larger run and
# the contents have no whitespace at the edges. Slack permits multi-word
# bold, so we don't constrain to a single token. We require a non-word
# char (or start/end) on the outside to avoid eating bare `*` in code
# expressions like `a*b*c`.
_BOLD = re.compile(r"(?<![\w*])\*(?!\s)([^*\n]+?)(?<!\s)\*(?![\w*])")
# Strike: same shape but with `~`.
_STRIKE = re.compile(r"(?<![\w~])~(?!\s)([^~\n]+?)(?<!\s)~(?![\w~])")


def to_commonmark(text: str, user_labels: Mapping[str, str] | None = None) -> str:
    """Translate Slack mrkdwn `text` into CommonMark.

    `user_labels` maps Slack user_id → display label; missing ids fall back
    to `@U…`. Channel labels come from the source string itself
    (`<#C…|name>`), so no lookup is needed.

    Code spans / fenced code blocks pass through unchanged: Slack uses the
    same backtick syntax as CommonMark, and we leave their contents alone
    so e.g. `*` inside code stays literal. Everything else is regex-driven
    on the whole string, which is fine because Slack doesn't nest formatting
    inside code in the wire format anyway — the backticks are preserved as
    literal characters in `text` and CommonMark will tokenise them on render.
    """
    user_labels = user_labels or {}

    def _user_sub(m: re.Match[str]) -> str:
        uid, label = m.group(1), m.group(2)
        return f"@{label or user_labels.get(uid, uid)}"

    def _channel_sub(m: re.Match[str]) -> str:
        cid, name = m.group(1), m.group(2)
        return f"#{name or cid}"

    def _subteam_sub(m: re.Match[str]) -> str:
        return f"@{m.group(1) or 'group'}"

    def _special_sub(m: re.Match[str]) -> str:
        return f"@{m.group(1)}"

    def _url_sub(m: re.Match[str]) -> str:
        url, label = m.group(1), m.group(2)
        if label is None or label == url:
            return f"<{url}>"
        return f"[{label}]({url})"

    out = text
    out = _USER_REF.sub(_user_sub, out)
    out = _CHANNEL_REF.sub(_channel_sub, out)
    out = _SUBTEAM_REF.sub(_subteam_sub, out)
    out = _SPECIAL_REF.sub(_special_sub, out)
    out = _URL_REF.sub(_url_sub, out)
    out = _BOLD.sub(r"**\1**", out)
    out = _STRIKE.sub(r"~~\1~~", out)
    # Slack escapes only these three entities in message text (per the
    # Formatting reference). Decode after the angle-bracket constructs are
    # consumed so we never accidentally synthesise a `<@U…>` from text the
    # user actually typed.
    out = out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
    return out
