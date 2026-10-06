# calendar — what each upstream does

What the downloaders rely on, and how each fact was established. The
render model (one page per event, series and changed occurrence) is in
[`calendar-common/README.md`](../../calendar-common/README.md).

## The raw store

One row per event in the upstream's own shape, keyed
`{calendar_id}#{event id}`:

- `ics_objects` — an iCalendar object per `UID`: the event, or a series
  with its changed occurrences, exactly as CalDAV stores a resource.
  The `.ics` method splits a file into the same shape, carrying the
  `VTIMEZONE`s each event names.
- `google_events` — a Google event resource verbatim. A changed or
  cancelled occurrence is its own row naming its series.

`calendars.sync_token` is each calendar's resume position. It is per
calendar, so widening the `calendars` filter needs no scope record: a
calendar added to the list has no token and is listed whole.

## A window (`since` / `until`)

Measured live. Neither service can resume a time-bounded
listing from a sync token — Google refuses `syncToken` beside
`timeMin`/`timeMax`, and CalDAV's `sync-collection` has no time bound —
so a windowed calendar is listed whole every run, whatever the listing
no longer names is dropped, and the calendar's token is cleared.

- **Google**: `singleEvents=false` with `timeMin`/`timeMax` returns
  every series with an occurrence in the window (one from 2016 that
  still recurs, say), every one-off in it, and only the changed
  occurrences that fall in it. It still returns a `nextSyncToken`; it is
  not kept.
- **CalDAV**: a `calendar-query` with a `time-range` on `VEVENT`, and
  `limit-recurrence-set` on `calendar-data`, which trims each series to
  the overrides in the window. On Fastmail one week of a busy calendar
  went from 109 overrides to 13 with it.

## Fastmail (CalDAV) — measured against a live account

Which credential to use, and how to make it read-only, is in
[`docs/dev/fastmail.md`](../../../../../docs/dev/fastmail.md).

- **Discovery starts at `https://caldav.fastmail.com/dav/`.** The bare
  host answers a `PROPFIND` with 404; `/.well-known/caldav` 301s to
  `/dav/calendars`. The `fastmail` method hardcodes the first, and the
  `caldav` method falls back to `.well-known` (following redirects
  itself — the transport does not).
- **Every property value comes wrapped in CDATA**, `calendar-data`
  included. A multistatus reader that only takes text events reads
  every name as empty and every event as missing.
- The home listing holds the calendars plus `Inbox` and `Outbox`, whose
  `resourcetype` is `schedule-inbox` / `schedule-outbox`, not
  `calendar`. Only `calendar` collections are mirrored.
- `sync-collection` with an empty token returned every object of a
  2,000-event calendar in one reply, with no 507 truncation. Asked for
  fewer with `<limit><nresults>2</nresults></limit>`, it answers 2
  objects and a 507 on the collection, and the token it returns moves
  to the next page, so the truncation path is real; the address book
  does the same.
- An unknown or expired sync token is a 403 whose body names RFC
  6578's precondition: `<D:error xmlns:D="DAV:"><D:valid-sync-token/>`
  with a `responsedescription` of `Invalid sync-token`. Only that
  starts a listing from nothing; any other refusal fails the calendar.
- Depth makes no difference: `sync-collection`, `calendar-multiget`
  and `calendar-query` returned the same at Depth 0, 1 and none, on
  each of 6 calendars. We send what the RFCs define (0, 0 and 1).

The loop is `datalib_etl::dav::sync`, which CardDAV shares, and what a
listing owes the store across runs is `datalib_etl::dav::state`. A
listing that stops short leaves a `listing:calendar <name>` problem and
deletes nothing until a later run carries it to the end. A server
without `sync-collection` fails the calendar: there is no second way
to list one, apart from the window's `calendar-query` above.
- Shape of the data across ~5,200 objects: 3,855 single events; the
  rest a series with 0 to many overrides in the same object; 5 objects
  holding only overrides (invitations to one occurrence). No `STATUS`
  properties at all — cancelled occurrences are `EXDATE`s. Every
  `TZID` was an IANA name.

## Google Calendar — measured against a live account

latchkey's `google-calendar` service holds the OAuth token
(`latchkey auth browser google-calendar`). The events call is
`singleEvents=false&showDeleted=true`: a series is one resource with
`recurrence`, and changed and cancelled occurrences are resources with
`recurringEventId` and `originalStartTime`. A `410 Gone` means the sync
token expired: the calendar is listed whole again and whatever the new
listing does not name is dropped.

- **Every events reply carries `items`**, `[]` when there is nothing:
  a whole listing, an empty window and an incremental sync with no
  changes alike, across 7 calendars. A reply without it is not a page,
  and fails the calendar rather than reading as an empty one.
- **An invalid or expired sync token** is a `410` whose error reason is
  `fullSyncRequired`.
- **Every event had an `id`**, across about 7,500 on the first pages of
  7 calendars. One without could be any stored event, so a whole
  listing that meets one deletes nothing and says so on the calendar's
  `listing:` row.
- A whole sync of the 7 calendars stored 10,332 events in 10 requests
  (two calendars ran past one 2,500-event page); the next, incremental,
  changed nothing in 8.
- **Pages come in no particular order.** A full listing names a deleted
  series as a `cancelled` resource, and its occurrences may come on
  either side of it; the deleted series is carried across pages so none
  of them is stored as an orphan.
- **A cancelled occurrence carries its full fields**, not the stub the
  reference shows.
- **`dateTime` is in the calendar's offset; `timeZone` is the event's
  own zone.** A flight made in Tokyo on a New York calendar comes back
  as `02:00-05:00` beside `Asia/Tokyo`. The wall clock is read in the
  named zone before it is shown.
- **Most descriptions are HTML** (1,276 of 1,492 on one calendar); render
  turns them to text.
