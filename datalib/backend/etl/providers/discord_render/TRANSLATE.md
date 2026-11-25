# Discord render

One document per conversation: a channel, a thread, a forum post, a
direct message. Discord's messages are not rooted in threads the way
Slack's are, so a channel reads as one transcript, like a WhatsApp
group's. A message that changes renders its whole conversation again.

## Where it sits

| | in | named |
|---|---|---|
| a channel | its server, itself as a place | `#bridge` |
| a thread or forum post | its server, its channel | its own name |
| a direct message | nothing above it | who is in it, or a group's own name |

A channel is two things: the place its conversation and its threads are
in, and that conversation. They have an id each (`ids.rs`).

`org_name` is the server. Ids take no account: a snowflake is unique
across the service, so two accounts that can read a message mirror one
message.

## A message's text

Discord's markdown is nearly CommonMark. `markdown.rs` writes over what
differs and escapes what Discord shows as typed:

| typed | becomes |
|---|---|
| a line break | a hard break: Discord breaks the line |
| `__x__`, `~~x~~` | `<u>x</u>`, struck; CommonMark reads the first as bold |
| `# x`, `## x`, `### x` | `**x**`: a `##` in a body would be styled as a message's header |
| `-# x` | `<small>x</small>` |
| `> x`, `>>> x` | a quote, the second to the end of the message; a list or a code block in it stays in it |
| `<@id>` | a chip link naming the person |
| `<#id>`, `<@&id>`, `</cmd:id>`, `<:name:id>` | `#name`, `@name`, `/cmd`, `:name:` |
| `<t:…>` | a `<time>`, as every stamp is |
| a bare link | `<link>`, without the punctuation typed after it |
| `\|\|x\|\|` (a spoiler) | as typed, bars and all |
| `![x](link)` | `!` and the link: Discord draws no picture there |
| raw HTML, a table, `####` | as typed, escaped |

Only a web address becomes a link.

## What else a message shows

- **A reply** quotes the first line of what it answers, where the
  conversation holds it, and is a reply edge to it.
- **A forward** is quoted whole, its files listed with the message's own.
- **Embeds** (a link preview, a bot's card) are quoted: who, title,
  description, fields, footer. A preview's description over 300
  characters is left out, and so is an embed that says no more than the
  link in the text or no more than its site's name. No remote picture is
  drawn. A message AutoMod stopped says the rule and what matched it.
- **A bot's laid-out text** is its words; its buttons are not shown.
- **A poll** is its question and answers with their counts, its result
  the answer that won, a sticker its name.
- **What happened** (someone joined, a thread was started, a pin) is a
  system line. A message type this build does not know shows its text
  and is an `uncovered_type` problem at `info`.

## Authors, reactions, times

An author is their profile's display name, else their username, with the
handle `discord:<id>`. A webhook's author is the name on the message and
has no handle. The account's own messages are under its own name.

Discord counts reactions and names nobody. A reaction is the account's
own, under its name, and one more for everyone else, whose `count` is
how many (`3 others`).

A message's time is the one in its id, an instant with no clock (`Z`).

## Incrementality

The forward scan is the conversations with a message or a file edge that
changed. Everything else reaches a conversation through what it declared
reading: each user, channel, role and server it looked up, its files'
edges, and the account. A change to the account renders every
conversation again, since each names it.

Conversations are read 20 at a time, fewer than the shared page: a busy
channel is a great many messages.
