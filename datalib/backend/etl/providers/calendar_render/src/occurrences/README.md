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
`datalib-doltlite -readonly` like every store.

`occurrences`, one row per occurrence:

| column | what it holds |
|---|---|
| `id` | `<calendar_id>#<instance_id>`, the key |
| `calendar_id` | the calendar, as the raw store's `calendars.id` |
| `instance_id` | Google: Google's own id — `<series id>_20261022T160000Z`, or `<series id>_20261224` for an all-day one, the original start in UTC; a one-off event's own id. CalDAV and `.ics`: the `UID`, and for an occurrence of a series `<UID>_<its original start>` spelled the same way |
| `series_id` | the series it belongs to (Google's `recurringEventId`, the iCalendar `UID`); null for a one-off event |
| `original_start_time` | where the rule put it, before any move; null for a one-off |
| `start_time`, `end_time` | as the event has them: RFC 3339 with the offset its own zone has that day (`2026-11-02T10:00:00-08:00`), or `YYYY-MM-DD` for an all-day event, whose end is the day after its last, as Google writes it |
| `start_utc`, `end_utc` | the same instants in UTC, to sort and to compare a window against. An all-day event begins at midnight in the calendar's zone |
| `all_day` | 1 for an all-day event |
| `time_zone` | the IANA zone the event is written in, else the calendar's |
| `status` | `confirmed` or `tentative`; `confirmed` when the event does not say. Cancelled occurrences are not listed |
| `title` | its summary; a changed occurrence with none takes its series' |
| `event_row` | the raw store's row it was read from (`google_events.id`, `ics_objects.id`) |
| `payload` | Google: the occurrence as `singleEvents=true` returns it — the series' resource under the occurrence's id and times, without `recurrence`, `updated` and `etag` included; a changed occurrence's own resource. CalDAV and `.ics`: `{"ics": "<the VEVENT>"}`, the series' own for a date its rule gives |

`unlisted`: an event whose occurrences could not be worked out, with
why — a `TZID` the zone database does not know, a rule part this build
cannot read (a `FREQ` RFC 5545 does not have), no start. Nothing is
guessed; the Manage row counts them as warnings.

`occurrences_window`: one row, `from_utc` and `until_utc`.

## What the listing does

`calendar-common`'s `occurrences.rs` runs one series forward and
`recur.rs` follows its `RRULE`, by Stalwart's `calcard` crate; this directory reads the raw rows and
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
