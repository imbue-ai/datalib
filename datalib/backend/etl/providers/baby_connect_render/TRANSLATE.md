# Baby Connect: what becomes what

One document per child, from the `kids`, `users` and `entries` tables
(`../baby_connect/INGEST.md`). A child with no entry has no document.
Months are a grouping a search chooses, not a document of their own
(`docs/dev/plans/messages_not_documents.md`).

## The document

- **Title**: the child's name, `Wesley`.
- **Days**: a `##` heading per day, a `<time>` of the date, in order.
- **Entries**: one `msg` block each, in clock order, keyed by the
  entry's uuid. Its first line is the time, the kind of entry in bold
  (`categories.rs`: the web page's own codes, a kind every 50, its
  variants just above it) and who logged it. Then the app's sentence
  for it (`Txt`), then the note typed with it (`n`) as a block quote.

## The time

An entry's `Pdt` and `Utm` are a clock reading with no zone. The render
step's `time_zone` (an IANA name) makes it a moment, with that date's
daylight saving. A reading in the hour the clocks skip is moved past
it, which is what the clock on the wall read; one in the hour they
repeat is the first of the two. With no `time_zone` the reading is
taken as UTC and written `Z` (the clock unknown), the page says so, and
each document carries a warning problem row
(`entry_time_assumed_utc`), which the Manage row counts.

## Grid rows

| row | kind | notes |
|---|---|---|
| the child | `Child` | the document; created and modified are its first and last entry |
| an entry | the kind's name (`Nursing`, `Diary`, …) | `author` who logged it, with an `email:` handle when the account names their address; `channel` the child; the body is the sentence and the note |

## The tree

A child (`Child`) is the document's one container, both a place and
what is read together, as a chat is. Its entries sit in it, sorted by
their time.

## Ids

A child and an entry are keyed by Baby Connect's id. An entry's uuid
carries its time.
