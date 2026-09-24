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

## Fastmail (CalDAV) — measured against a live account, 2026-09-24

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
  2,000-event calendar in one reply, with no 507 truncation. The
  truncation path (a 507 on the collection itself) follows the new
  token anyway.
- Shape of the data across ~5,200 objects: 3,855 single events; the
  rest a series with 0 to many overrides in the same object; 5 objects
  holding only overrides (invitations to one occurrence). No `STATUS`
  properties at all — cancelled occurrences are `EXDATE`s. Every
  `TZID` was an IANA name.

## Google Calendar — built from Google's documentation, not yet run live

latchkey's `google-calendar` service holds the OAuth token
(`latchkey auth browser google-calendar`). The events call is
`singleEvents=false&showDeleted=true`: a series is one resource with
`recurrence`, and changed and cancelled occurrences are resources with
`recurringEventId` and `originalStartTime`. A `410 Gone` means the sync
token expired: the calendar is listed whole again and whatever the new
listing does not name is dropped. **These shapes come from the API
reference; check them against a live account before trusting a
fixture built on them.**
