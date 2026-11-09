# Searching your mirror

One query language answers everywhere you can search datalib: the
search box at the top of the app (Enter runs it in the search tab you are on,
or a new tab from anywhere else; ⌘Enter always opens a new tab; and its arrow
opens Advanced search, whose every setting writes the same
words into the box), `datalib-applet search <data_root> '<query>'`
from a shell (no app or server needed; it prints JSON), and
`GET /applet/unified_document_index/search?q=…` on a running
`datalib-http`. This page is that language. `datalib-step help search`
prints it, and a release ships it beside the binaries as
`docs/user/searching.md`.

```sh
datalib-applet search ~/datalib 'from:me after:2026-10-20'
datalib-applet search ~/datalib 'warp core -from:riker is:document'
```

## Words, and how they are matched

A query is free text and `key:value` terms, in any order; everything
must match. Free text looks for words in your documents:

- **By their fields:** every word as the start of a value a row is filed
  under: a name, an address, an id, a title, a label.
- **By their words:** the words themselves, ranked by how well each document
  matches.
- **By their meaning:** the documents closest in meaning, even with none of
  your words in them. With a key that keeps only some of a document's messages
  (`from:riker`, a day), the document is judged by those messages alone.

All three are merged into one list, documents that rank well in more than one
first.

Every document has some similarity to any words, so meaning needs a cutoff, or
every search would find every document:

| you write | meaning |
|---|---|
| nothing | documents at a similarity of at least 0.25 count |
| `semantic:0.4` | ask for a closer match; from 0 to 1, where 0 keeps every document |
| `semantic:off` | leave meaning out. Faster from a shell, which then loads no model |

A word or a field that matches counts however far off its meaning is.

A search can be in another language than the document. Meaning crosses
languages: a question in German or Italian finds an English document,
and the other way round, though often at a lower similarity, so lower the
cutoff if a foreign question finds little. Words and fields match what you
typed, so across languages they find only what the two spellings share, such as
a name. Meaning covers a source only once its `embed` step has run, so a source
with that step turned off answers in the words it is written in. A line above
the results says how many of the documents the search could have found have no
vectors, and hovering says which sources they are in. From a shell or the API
it is `query_echo.without_vectors`. While a sync is writing the index, meaning
and that line answer from what the search last read, so a
search stays quick; what the sync added counts once the index has been still
for half a minute.

A search covers the sources the document index holds. A source turned off can
leave it after an upgrade changes how documents are stored, because a turned-off
source does not re-render, and a warning on its row says so. Turn it on and sync
it to bring it back; what it has already embedded is kept.

Free text made only of ids and addresses (`picard@enterprise.org`,
`tel:+12025550101`, a row's uuid) is looked up directly: every row that names it, the ones it is most about first. A source's
own id for a record counts too, where it is a uuid, dashed or not, and so
does a link that ends in one: paste a Notion page's link to find the page.

A source's words are searchable once its `keyword_index` step has run;
until then free text finds nothing there and the answer says why.

## Keys

| you write | finds |
|---|---|
| `key:value` | rows whose `key` is `value` |
| `-key:value` | rows whose `key` is not `value` |
| `key:"two words"` | a value with a space in it |
| `key:*` / `-key:*` | rows with any value there / with none |
| `before:2026-10-01`, `after:2026-10-01T09:00` | rows last active before or after then — a thread's last message, a page's last edit, an event's start; a time with no zone is your machine's |
| `is:document` / `-is:document` | one row per document (a thread, a conversation, a page) / only the rows inside documents |

The keys for what a row is and where it lives are `source:` (a
source by its id in the config; also `source_id:`), `kind:` (`Email`,
`Slack Message`, …), `subject:` (also `conversation:`), `channel:`,
`account:`, `project:`, `convo:` (a document by its id, and every row in it), `contact:` and
`email:` (an address book's columns), and the id of any other column the
grid shows (`org_name:`, `created_at:`, …). These match the whole value
as stored, case and all: `kind:Email`, not `kind:email`.

### Dates and words together

`after:` and `before:` keep each message by its own time, and a
document (a thread, a chat, an agent session) by its last message. A
chat that runs over several days is kept for any day it was active,
and without free text you see only that day's messages. With free text, meaning
ranks the chat by its messages in the window alone, while words rank the whole
chat by every word in it, then open it at the message in the window holding the
most of your words. So a chat can rank for words it said on another day, and
open on a message that does not hold them.

`GET …/search/keys` lists every key a root has, and
`…/search/values?key=…&typed=…` the values one takes, most rows first.
A key the search does not have is refused by name rather than ignored,
so a typo never quietly matches everything.

## People

A person is found by the part they played:

| key | finds rows where the person |
|---|---|
| `from:` (also `author:`) | wrote it |
| `to:`, `cc:`, `bcc:` | was in that header |
| `recipient:` | was in any of them |
| `mention:` | was mentioned, where the source marks mentions up |
| `with:` (also `involves:`) | played any of those parts, or took part in the conversation the row is or is named after (a direct message's other party, even where only you wrote), or is who an address-book card is about |
| `label:` | (not a person) an email's labels; a photo's keywords (and the keywords above them), the people tagged in it and its albums |
| `place:` | (not a person) where a photo was taken: a city or a landmark; or, given an [H3](https://h3geo.org) cell at any resolution (`place:86283082fffffff`), every row whose location falls in it |
| `near:` | (not a person) given an H3 cell, every row in it or in one of its six neighbours: what the photo card's "photos in this area" writes. `place:` with a cell can miss a row just inside the cell's edge, because H3's coarse cells do not exactly hold their finer ones; `near:` never does, and also finds some rows just outside |
| `caption:` | (not a person) a photo's caption; the free text in the search box does not reach photos, which have no page |

The value can be:

- **An address or number** (`riker@enterprise.org`, `+12025550101`), or
  any handle as datalib writes it (`email:…`, `tel:…`,
  `slack:T…/U…`, `github:jlpicard`): that person exactly.
- **A quoted name** (`from:"Will Riker"`): any name holding those
  words in that order, each whole (`from:"Riker"` finds "Will Riker
  <riker@…>", `from:"Rik"` does not), and every handle a source showed
  under exactly that name. `label:"Away"` likewise finds the label
  "Away Missions".
- **A bare word** (`from:riker`): any name or handle holding it, so it
  finds Riker under every spelling a source used, and anyone else whose
  name holds it.
- **`contact:<id>`**: every handle linked to that contact in the
  contacts app, and its members' for a group. The app writes this for
  you when you pick a person from the search box's suggestions.
- **`me`**: you. See below.

## `me`

`from:me`, `to:me`, `with:me` and the rest find you, in every source
that can say which person is you. `me` matches:

- the "Me" a source shows you as, where it shows you that way (a
  phone's message backup, an AI chat), and, in a phone's backup, the
  recipient of every message someone else sent you;
- every handle a source says belongs to the account it mirrors;
- every handle linked to a contact you marked as you: tick *This is
  me* under *Your contact* on its person card. Link an old address, a
  second number or your Notion user to that contact and `me` finds
  them too, from the next search on.

A handle counts in every source once one source has said it is yours.
If your mailbox is `picard@enterprise.org`, a calendar event organized
by that address, or a Google Chat message from it, is yours too.

A quoted `"me"` is the word, not you.

Your number or address is narrower than `me`: `from:+12025550100`
finds only what a source records as sent from that number, not
everything you wrote. A phone's backup records the number an MMS was
sent from, but not a plain text's, so `from:me` finds your texts and
your number finds only your MMS.

| source | what `me` knows you by | what it cannot find |
|---|---|---|
| Email | the mailbox's address | |
| Slack | the signed-in user, and the address on their profile | `to:me`: Slack has no recipients; use `mention:me` or `with:me` |
| Beeper | the account's own person, by the handles Beeper lists | |
| GitHub, GitLab | the signed-in user, on pull and merge requests they opened or commented on | |
| Google Drive | the account's address, on comments it wrote | |
| Google Takeout (Chat) | the account's address | |
| LinkedIn | the owner's main address, on their posts and comments | their messages, which LinkedIn files under their profile link |
| Facebook | the owner's name and their profile's first address | |
| Claude, ChatGPT | "Me", and the account's address | |
| WhatsApp, Signal, Messages, SMS backup, Google Voice | "Me", as the author of what you sent and the recipient of what you received (texts, and Google Voice's voicemails; not call logs) | your number, where another source names you by it: these backups do not say it, except as below |
| SMS backup, besides | every number you sent an MMS from, each phone or SIM you used | a number you only ever sent plain texts from, which the backup does not record; one Android wrote as `insert-address-token` |
| Claude Code, Codex | "Me" | |
| Notion, calendar and the rest | an address another source already marks as yours, or a handle linked to a contact you marked as you | anything under another name |

Two more limits, whatever the source:

- **`to:me` needs you as a recipient.** Email lists you in its headers,
  and a phone's backup received every message in it that you did not
  send. Slack, Beeper, Google Chat, Messenger and LinkedIn messages
  name no recipient: a message there reached everyone in the
  conversation, so use `mention:me`, or `with:me` for the conversations
  you were in.
- **"Me" takes part in no conversation.** You are in every conversation
  in your own backup, so it would tell none apart: `is:document with:me`
  lists the email and Slack threads you were in, but not a chat where
  you appear only as "Me". Your messages in it are still found.

An address you no longer use, or a handle no source can tell is yours
(a second phone, an old login), is `me` once it is linked to a contact
you marked as you.

## Sorting and grouping

A search with words comes best match first, and one without newest first.
The app's **Sort** control and the table's column headers ask for another
order; so do `--sort` and `sort=`:

```sh
datalib-applet search ~/datalib 'with:riker' --sort touched_at:desc
datalib-applet search ~/datalib 'label:"Away"' --sort byte_size:desc,author_ref
```

A sort names a column of the grid and a direction, `:asc` (the default) or
`:desc`, each column breaking the ties of the one before: `touched_at`,
`created_at` and `modified_at` for dates, `byte_size` for size, `author_ref`
for author, `subject_ref` for title, and any other column the table shows.
Text ignores case, and a row with nothing there comes last in either
direction, so `touched_at:asc` opens on the oldest row that has a date. A
sort this search does not have comes back in `errors`, and the rows in the
default order. A row has several labels and people, so neither is a sort.

Sorting a search with words reorders everything it found, down to the cutoff
above. Raise the cutoff if a date-sorted search shows too much.

The table's **group by** does the same with `--within` and `/search/groups`:
every group with its count, then each group's rows a page at a time.

## When the search says no

A part of the search that cannot answer costs that part, not the rest: while
the embedding model is still downloading on a first launch, you still get what
your words and fields match, with a line above the results saying what is
missing, `query_echo.partial`. `datalib-applet search` prints those rows and
exits 1.


The answer's `refused` says why a query was not run: `tab=` or `--tab`, which a
search no longer has, a key the search
does not have, `is:` with a word it does not know, a `contact:` on a
root without the contacts app, or `me` on a root whose index has not
synced since it learned which handles are yours. `datalib-applet
search` prints the reason and exits 1, so a script cannot take a
refusal for an empty result.
