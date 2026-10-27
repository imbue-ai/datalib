# A calendar's occurrences, one row each

The raw store keeps what the service sends: a series once, with its
repeat rule, and each occurrence somebody moved, edited or cancelled as
a row (Google) or a `VEVENT` (CalDAV, `.ics`) of its own. The rendered
pages keep it that way (`calendar-common/README.md`). What a person or
an agent usually asks — *what is on my calendar next week* — needs the
other shape: one row per occurrence, the way Google's
`events.list?singleEvents=true&timeMin=…&timeMax=…` answers. The
`render_occurrences` step writes that shape.

## The step

```toml
[[steps]]
group = "calendar"
function = "render_occurrences"
inputs = ["calendar/ingest"]

[steps.params]
days_back = 30    # the default
days_ahead = 400  # the default
```

It lists every occurrence that overlaps the window from midnight UTC
`days_back` days before the run's day (`DATALIB_DAG_NOW`) to the end of
the day `days_ahead` after it, and replaces whatever the store held.
Whole days, so two runs on one day write the same rows.

**The window moves when the step runs**, and like every derived step it
runs when what it reads moves: when `ingest` stored something new. A
calendar nobody touches for a week keeps last week's window. Read
`occurrences_window` to know which days the rows cover.

## The store

`<group>/render_occurrences/occurrences.doltlite_db`, read with
`datalib-doltlite -readonly` like every store. Its three tables
(`occurrences`, `occurrences_window`, `unlisted`) and every column are
in [`raw_stores.md`](../../../../../../../docs/user/raw_stores.md)
§ "A calendar's occurrences", the guide the release ships.

A Google `payload` writes its `dateTime`s in the calendar's zone, as
Google's own listing does; the `start_time` columns use the event's
zone. Both kinds of row get the same treatment: a date the rule gives
and a changed occurrence copied from the raw store.

## What the listing does

`calendar-common`'s `occurrences.rs` runs one series forward and
`recur.rs` follows its `RRULE`; this directory reads the raw rows and
names what comes out. The traps it handles, each with a test:

- **The series' own zone.** A rule runs on the wall clock of the zone
  its first start is written in, so a weekly 09:00 stays 09:00 across
  daylight saving, and each occurrence carries the offset its own day
  has.
- **`UNTIL`** is compared the way it was written: in UTC against the
  occurrence's instant, a bare date against its day, a floating time
  against its wall clock.
- **`EXDATE`** with a `TZID`, in UTC, or as a bare date, and Google's
  `EXDATE;TZID=…` lines in `recurrence`, all take out the same date.
- **Changed occurrences.** A moved or edited one replaces the date it
  came from and is listed where it went, in or out of the window; a
  cancelled one takes its date away. An invitation to one occurrence of
  a series the calendar does not hold is listed alone.
- **Google's split series.** "This and following" ends the old series
  with an `UNTIL` and starts `<id>_R<start>`; its occurrences, and the
  rows that change them, are `<id>_<start>Z`, without the `_R…`.
- **All-day series** run on dates and are named by date.
- **Google stops a rule that never ends after 730 occurrences.** A
  Google series with neither `COUNT` nor `UNTIL` is given `COUNT=730`,
  which is what Google's own listing does (measured on daily, weekly
  and monthly series): a daily meeting begun in 2020 is over in July
  2022. A CalDAV or `.ics` series runs as RFC 5545 says, forever.
- **`COUNT` counts the first start**, and the first start is an
  occurrence even when the rule would not give it (RFC 5545).
