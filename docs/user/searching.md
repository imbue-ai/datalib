# Searching your mirror

One query language answers everywhere you can search Datalib: the
search box at the top of the app (Enter runs it in the search tab you are on,
or a new tab from anywhere else; ⌘Enter always opens a new tab; and its arrow
opens Advanced search, whose every setting writes the same
words into the box), `datalib-applet search <data_root> '<query>'`
from a shell (no app or server needed; it prints JSON), and
`GET /applet/unified_grid_and_search_index/search?q=…` on a running
`datalib-http`. This page is that language.

```sh
datalib-applet search ~/datalib 'from:me after:2026-10-20'
datalib-applet search ~/datalib 'warp core -from:riker is:document'
```

## Words, and how they are matched

A query is free text and `key:value` terms, in any order. Its terms and
some of its free text are **filters**, which every result must pass; the
rest of its free text is **words to rank by**.

| you write | it is |
|---|---|
| `key:value`, `-key:value` (below) | a filter on a row's fields |
| `"two words"` | a filter: the row's text holds those words, in that order. Also ranked by |
| `+word` | a filter: the row's text holds the word. Also ranked by |
| `-word`, `-"two words"` | a filter: the row's text does not hold it |
| an id, a link ending in one, an address or a number (below) | a filter: the row names it |
| any other word | ranked by |

A row's text is its message, for a row inside a conversation (a thread, a
chat, an agent session), and the whole document for a document's own row.
So `warp -breach` finds a thread whose other message says "breach", at the
message that does not; a thread every message of which says "breach" is
not found at all.

Every filter is applied before anything is ranked. The words are ranked
two ways, and the two lists merged, documents that rank well in both
first:

- **By their words:** a conversation matches when one of its messages holds
  every word, and opens at that message. A page (a document, a calendar, a
  device's readings) also matches when its parts hold the words between
  them, ranked below every document that says them in one place. Words
  typed together in that order rank first.
- **By their meaning:** the documents closest in meaning, even with none of
  your words in them. With a filter that keeps only some of a document's
  messages (`from:riker`, a day, `-breach`), the document is judged by
  those messages alone.

There is no `OR`: typed between two words it is searched as the word
"or", so `sam OR samuel` asks for documents holding both names. Search
for each separately. A search without words to rank by lists what its
filters keep, newest first.

Words match as written, ignoring case and accents: "runs" does not find
"running", and a quoted phrase is exact. Meaning finds the other forms of
a word, and its synonyms.

Every document has some similarity to any words, so meaning needs a cutoff, or
every search would find every document:

| you write | meaning |
|---|---|
| nothing | documents at a similarity of at least 0.25 count |
| `semantic:0.4` | ask for a closer match; from 0 to 1, where 0 keeps every document |
| `semantic:off` | leave meaning out. Faster from a shell, which then loads no model |

A document whose words match counts however far off its meaning is.

A search can be in another language than the document. Meaning crosses
languages: a question in German or Italian finds an English document,
and the other way round, though often at a lower similarity, so lower the
cutoff if a foreign question finds little. Words match what you typed, so
across languages they find only what the two spellings share, such as a
name.

### What a search covers

A source's words are searchable once `unified_grid_and_search_index` has run
after its render; until then words to rank by, and the quoted, `+` and `-`
filters, find nothing there, and the answer says why. A source that step's `not_searched` lists is never searched. Meaning covers
a source only once its own `embed` step has run, so a source with that step
turned off is found by its words alone. A line above the results
says how many of the documents the search could have found have no vectors,
and hovering says which sources they are in; from a shell or the API it is
`query_echo.without_vectors`. While a sync is writing the index, meaning and
that line answer from what the search last read, so a search stays quick; what
the sync added counts once the index has been still for half a minute.

A turned-off source does not re-render, so it can drop out of search when an
upgrade changes how documents are stored; a warning on its row says so. Turn
it on and sync it to bring it back; what it has already embedded is kept.

### Ids as free text

An id, an address or a number typed as free text (`picard@enterprise.org`,
`+12025550101`, a row's uuid) is a filter: every row that names it, in any
role, newest first. A source's own id for a record counts too, where it is
a uuid, dashed or not, and so does a link that ends in one: paste a Notion
page's link to find the page. A source id that is not a uuid, such as a
Gmail id, is not read this way; search for it with `id:` (below).

### The text a search reads

What the words and the meaning search over is each item drawn plainly:
who wrote it (with their address, where it is an email or a phone
number), to whom, when, its labels, then what it says, with links as
their text. `datalib-step index-text <data root> --source <id>` prints it
for every document of a source; `--document <id>` for one, `--at
<commit>` as it was at a commit of the source's render store, and
`--chunks` as the pieces the meaning embeds.

## Keys

| you write | finds |
|---|---|
| `key:value` | rows whose `key` is `value` |
| `-key:value` | rows whose `key` is not `value` |
| `key:"two words"` | a value with a space in it |
| `key:*` / `-key:*` | rows with any value there / with none |
| `before:2026-10-01`, `after:2026-10-01T09:00` (`since:` is `after:`) | rows last active before or after then — a thread's last message, a page's last edit, an event's start. See "Writing a time" below |
| `is:document` / `-is:document` | one row per document (a thread, a conversation, a page) / only the rows inside documents |

The keys for what a row is and where it lives are `source:` (a
source by its id in the config; also `source_id:`), `kind:` (`Email`,
`Slack Message`, …), `subject:` (also `conversation:`), `channel:`,
`account:`, `project:`, `id:` (a row by the id its source gave it),
`datalib_document_id:` (a document by Datalib's id for it, and every
row in it), `contact:` and `email:` (an address book's columns), and
the id of any other column the grid shows (`org_name:`, `created_at:`,
…). These match the whole value as stored, case and all: `kind:Email`,
not `kind:email`. A uuid is the one exception: 32 hex digits, with or
without the four dashes, match the same uuid however it is written and
however its source stored it, dashed or not, in capitals or not.
`datalib_document_id:` and `notion_page:` also take their uuid at the
end of a slug or a link, so a Notion page's link can follow
`notion_page:` as it is.

`id:` takes the id as the source spells it: an email's id in Gmail or
on a JMAP server (not its `Message-ID` header), a Notion page's id, a
Claude or ChatGPT conversation's. A Notion page's id can be copied from
the end of its link, where Notion leaves out the dashes:
`id:1701d000abcd4e00a0000000000000ef` finds the page whose id Notion's
API writes `1701d000-abcd-4e00-a000-0000000000ef`. Any id that is not a
uuid matches only as written, case and all, since some sources' ids
differ only in case (two comments on one Google Doc can). Where one id
is not enough to name a record, the value joins several with `#`: a
Slack message is `<channel>#<ts>`, a GitHub comment
`<owner>/<repo>#<id>`. Two sources can use the same id, so add
`source:` when that matters.

`id:` is the exact match, unlike an id pasted as free text (above):
the rows whose source id is that value, and nothing else.

`datalib_document_id:` takes the id Datalib gave a document (a thread,
a conversation, a page) and keeps that document and every row in it,
over every month of a chat; Keep only on a Subject cell writes it. A
message's own id finds nothing, since a message is a row inside a
document.

### Writing a time

`before:`, `after:` and `since:` take any of these. A time with no zone
is your machine's.

| you write | it means |
|---|---|
| `2026-10-01`, `2026/10/01` | midnight starting that day; the second is how Gmail writes a day |
| `2026-10-01T09:00`, `2026-10-01T09:00:00-07:00` | that moment |
| `-30d`, `-12h`, `-2w` | that long before now: days, hours or weeks, with the sign, as a source's `since` is written |
| `"Tuesday, April 3, 2029"`, `"3 Apr 2029 at 9pm"`, `"Tue Apr 3rd, 2029 at 08:30"` | a day, or a day and a time, in words: in quotes, with the year. A weekday the date does not fall on is refused |

Anything else is refused, with a line saying why, rather than ignored.
`since:-30d` is the last thirty days.

### Dates and words together

`after:` and `before:` keep each message by its own time, and a
document (a thread, a chat, an agent session) by its last message. A
chat that runs over several days is kept for any day it was active,
and without free text you see only that day's messages. With free text,
both its words and its meaning rank the chat by its messages in the
window alone, and it opens at one of them.

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
| `with:` (also `involves:`) | played any of those parts, or took part in the conversation the row is or is named after (a direct message's other party, even where only you wrote), or is who an address-book card is about, or is tagged in a photo |
| `pictured:` | is tagged in a photo, on a face or by a person keyword. A tag is a name and no address, so the value matches the name |
| `label:` (also `in:`, as Gmail writes `in:sent`) | (not a person) an email's labels and mailboxes; a photo's keywords (and the keywords above them), the people tagged in it and its albums |
| `place:` | (not a person) where a photo was taken: a city or a landmark; or, given an [H3](https://h3geo.org) cell at any resolution (`place:86283082fffffff`), every row whose location falls in it |
| `near:` | (not a person) given an H3 cell, every row in it or in one of its six neighbours: what the photo card's "photos in this area" writes. `place:` with a cell can miss a row just inside the cell's edge, because H3's coarse cells do not exactly hold their finer ones; `near:` never does, and also finds some rows just outside |
| `caption:` | (not a person) a photo's caption. Free text finds it too, on the photo's page, unless the photo's source is in `not_searched` |
| `keyword:` (also `tag:`) | (not a person) a photo's keywords, and the keywords above them in the keyword tree |
| `rating:` | (not a person) a photo's stars, `1` to `5`: `rating:4` exactly, or compared, `rating:>=3`, `rating:<2` (`>`, `>=`, `<`, `<=`). `-rating:*` is the unrated ones |
| `collection:` (also `album:`) | (not a person) the collections a photo is in: in Lightroom its collections, the Quick Collection, a saved book, slideshow, print or web gallery, and a publish service's collections (Flickr, Hard Drive, …). A collection inside a set answers to its own name, each set's name above it, and its whole path, `"Trips › Japan"`. A smart collection is a saved search, not a list of photos, and is not one |

The value can be:

- **An address or number** (`riker@enterprise.org`, `+12025550101`), or
  any handle as Datalib writes it (`email:…`, `tel:…`,
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
your words match, with a line above the results saying what is missing,
`query_echo.partial`.

The answer's `refused` says why a query was not run: a key the search
does not have, `is:` with a word it does not know, a `contact:` on a
root without the contacts app, `me` on a root whose index has not
synced since it learned which handles are yours, or `tab=` / `--tab`
(a search has one answer, every way of matching merged).

Either way `datalib-applet search` prints what it has and the reason, and
exits 1, so a script cannot take a refusal or a partial answer for an
empty or a whole one.
