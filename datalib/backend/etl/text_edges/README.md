# The text-edge library

Text someone else wrote reaches a rendered document, and from there the
page and search. Every way that has gone wrong is a case in
[`cases.toml`](cases.toml): the text, how it arrives, and what the page
and search should make of it. When a new one turns up, in feedback, a
bug or a real library, add it here.

## What a case is

```toml
[[case]]
id = "placeholder-in-a-fence"   # unique; names the test
edge = "What went wrong, and in which PR."
enters = "markdown"             # how the text arrives; see below
text = '''…'''                  # the text, as its source sends it
shows = ["…"]                   # the page shows this, as typed
hides = ["…"]                   # the page does not show this
finds = ["…"]                   # a quoted search for this finds the page
elements = { pre = 1 }          # elements the text adds to the page
fails_today = "why"             # optional: this case fails on the tree as it is
```

Every case is made up, in the shape a real library showed: never paste
what a real message says (AGENTS.md §"Real data stays out of the
repo").

`enters` is the path production takes for that kind of text:

| `enters` | the text | rendered by |
|---|---|---|
| `markdown` | Markdown an assistant or a person wrote: Claude, ChatGPT, Claude Code, Codex, Todoist | the text step those renderers share, then chat-common |
| `slack` | a Slack message, as Slack's API sends it (`&lt;` for a typed `<`) | `to_commonmark`, then chat-common |
| `forge` | a GitHub pull request's description, with a comment after it | forge-render-common |
| `email_html` | an email's HTML body | email's HTML-to-markdown step, then chat-common |
| `plain` | a text message: Signal, WhatsApp, LinkedIn | chat-common's plain text |

## How a case is judged

`render_hostile_samples` (`../hostile_samples/`) renders each case's
text as the first of two messages, with Picard's "Make it so." after it,
and writes the document as the document route sends it: the body, and
the section layout where the renderer declares one. `//datalib/ui:unit_test`
then renders that through the app's own markdown-it, sanitizer and
per-message fragments, in
[`text_edges.test.ts`](../../../ui/tests/text_edges.test.ts), and checks:

- **`shows` and `hides`** against the words of the case's own message,
  or of the whole page where the renderer declares no messages.
- **`elements`**: how many more of each element the page has than the
  same path draws for the text `Engage.`.
- **`finds`**: the phrase's words, in order and side by side, in the
  `.md` file as qmd's keyword index splits it, and in the grid's text
  (`plain_text`).
- **Always**: the next message shows whole, outside any code, as the
  only element answering to its id; and no script, frame, form or `on…`
  attribute survives.

A case marked `fails_today` must still fail. When a change fixes it, the
test says so, and the mark comes off in the same change. So the marks
list what is broken now, and nothing else.

Run it:

```sh
bazelisk test //datalib/ui:unit_test --test_arg=tests/text_edges.test.ts --test_output=errors
```
