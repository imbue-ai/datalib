# calendar-common — one layout for every calendar

A calendar provider turns what its upstream sent into
`NormalizedEvent`s; this crate turns each one into a markdown page, a
`grid_rows` row and the `edges` that link it to its relatives. Today the
one provider is `calendar`, which has four ways in — Google's API,
Fastmail over CalDAV, any other CalDAV server, and `.ics` files — and
two raw shapes (iCalendar text and Google's JSON). Both normalize here,
so an event reads the same whichever way it arrived.

## One document per what, exactly

| What upstream has | Documents | Grid `kind` |
|---|---|---|
| A one-off event | one | `Event` |
| A recurring series (`RRULE`, Google's `recurrence`) | one, saying how it repeats | `Recurring Event` |
| An occurrence of a series that was moved or edited (`RECURRENCE-ID`, Google's `recurringEventId`) | one, linked both ways with its series | `Changed Occurrence` |
| An occurrence that was cancelled (`EXDATE`, a cancelled override) | none — a line on the series page | — |
| An occurrence the rule produces and nobody touched | none | — |

**Unchanged occurrences are never written out.** A weekly meeting with
no end date has infinitely many, and even a bounded window ("the next
year") would make the render depend on *today*: the same raw store would
render differently tomorrow, re-render every series every day, and fail
the fixture's byte-identical convergence. The rule on the series page is
the whole truth about them; expanding it is a query-time job (a
calendar view in the UI), not a stored one.

**A changed occurrence is a document** because it carries information
the rule does not: a new time, a new room, a different agenda, different
attendees. It links to its series (`Series: <title>`) and the series to
it (`Changed: <date>`) through `edges`, so the preview pane walks either
way. An occurrence of a series the store does not hold — an invitation
to one date of someone else's meeting — stands alone and says so.

## Dates in the grid

`created_at` is **when the event happens**: the start's instant, so
`before:` and `after:` mean "happening then" and the grid sorts a
calendar the way a calendar reads. For a series that is its first
occurrence; for a changed occurrence, its new start.

`modified_at` is **null**. Every other provider's document is created
and then edited, and the index holds them to that order
(`ingested_tng_test` checks `created_at_utc <= modified_at_utc` on every
document row); an event is nearly always last edited *before* it
happens, so its edit stamp in that column would break the order for
almost every future meeting. When the event was added (`CREATED`) and
last changed are on the page and in its frontmatter instead.

The id is deliberately **not** stamped with that start (`datalib_id`'s
leading bits): rescheduling a meeting must not re-key it and orphan its
feedback and links. A one-off event that later gains an `RRULE` keeps its
id too — both are the upstream kind `event`.

## Time zones

`EventTime` keeps a time the way its source wrote it and resolves the
instant only when asked (`when.rs`):

- A `TZID` is looked up in the IANA database (`chrono-tz`). A name it
  does not know — an Outlook zone, a typo — is not guessed: the start
  is nulled on the grid row and a `problems` row says why; the page
  still shows the wall-clock time and the name.
- A floating time (no zone) is read in the calendar's own zone, else as
  UTC with a `problems` row recording the guess.
- An all-day date is its midnight UTC. `after:2026-09-13` then finds an
  event on the 13th whatever zone the reader is in.
- A time in a spring-forward gap moves past the gap (RFC 5545 §3.3.5).

## Repeat rules in words

`rrule::describe` writes the sentence the page and the grid's search
text carry: `FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=…` is "Weekly on Monday and
Thursday, until Thu 31 Dec 2026", with `UNTIL` shown as the date it
falls on in the series' own zone. The shapes it reads are the ones a
real Fastmail account held (see the test); anything it cannot phrase is
appended verbatim, and the raw rule is always on the page beside it.
