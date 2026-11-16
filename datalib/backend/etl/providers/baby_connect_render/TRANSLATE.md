# Baby Connect: what becomes what

One document per child per month, from the `kids`, `users` and `entries`
tables (`../baby_connect/INGEST.md`). A month with no entry for a child
has no document.

## The document

- **Title**: the child's name and the month, `Wesley — October 2048`.
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
| the month | `Baby Connect Month` | the document; created and modified are its first and last entry |
| an entry | the kind's name (`Nursing`, `Diary`, …) | `author` who logged it, with an `email:` handle when the account names their address; `channel` the child; the body is the sentence and the note |

## The tree

A child is a place (`Child`), on every month's document. Its months are
read-together containers under it, sorted by month; a month's entries
sit in the month, sorted by their time.

## Ids

A child and an entry are keyed by Baby Connect's id; a month by
`<child id>#YYYY-MM`. An entry's uuid carries its time.
