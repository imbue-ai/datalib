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
  cancelled occurrence is its own row naming its series. Its `etag`
  and `updated` are in the sidecar's `volatile_payload`: Google bumps
  them on hundreds of events at once with nothing else changed.
- `calendly_events` — a Calendly scheduled event as the listing sent
  it, its `event_memberships` and `event_guests` put in a fixed order
  (by user URI, by address). `listing_hash` is the blake3 of that
  payload. Its calendar is the signed-in user (below).
- `calendly_invitees` — the invitees of one scheduled event, keyed
  like the event, payload `{"invitees": [...]}` in booking order.

Calendly's own shape is kept, rather than turned into iCalendar or
Google's, because two of the things it holds have no place in either:
an event's invitees are a second request, with an outcome of their own
(so a failed fetch of them is a record that stays owed, not a hole in
the event), and each invitee carries their answers to the booking
form, which render shows as a "Booking answers" section. Render
projects both onto the shared event (`calendar_render/src/render/calendly.rs`).

`calendars.sync_token` is each calendar's `sync-collection` token. It
is per calendar, so widening the `calendars` filter needs no scope
record: a calendar added to the list has no token and is listed whole.

What a CalDAV listing named is `dav_resources`, one row per object with
the etag it was listed at; the etag the stored object satisfies is
`held_version` in that table's `_bookkeeping` sidecar, written in the
transaction that stored it. What is owed is the difference, asked of
the store each run and never stored: an object the listing named
without its data and a `multiget` did not return stays owed after the
token has moved on. A listing that stops short leaves what it never
reached in `dav_unconfirmed`, and nothing is deleted by absence until a
later run carries the listing to its end. Both tables are
`datalib_etl_web::dav::state`.

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

The loop is `datalib_etl_web::dav::sync`, which CardDAV shares: each page
of a listing is one transaction (what it names, what came with its
data, the deletions, the token), then what is listed and not held is
fetched by `multiget` through `datalib_etl_web::owed`. A listing that stops
short leaves a `listing:calendar <name>` problem and deletes nothing
until a later run carries it to the end. An object with no `VEVENT`
`UID` is held at its etag with a warning on its `dav_resources` row,
and not asked for again until the listing names it at a new etag. A
server without `sync-collection` fails the calendar: there is no second
way to list one, apart from the window's `calendar-query` above.
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
listing does not name is dropped. The listing carries whole events, so
nothing is owed after it: each page is one transaction, and the token
moves in the transaction that ends the listing, with its prune. A
listing cut off before its last page kept no token and starts again.

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

## Calendly — from the API reference, not yet run against an account

**Nothing in this section was checked against a live account.** No
Calendly token was at hand when it was written, so every shape below is
the one Calendly's published reference gives, and the fixtures in
`tests/calendar_tests/calendly_*.rs` are made up in that shape. The
first run against a real account is
`bazel run //datalib/backend/etl/providers/calendar:calendar_live`
(`tests/calendar_tests/live.rs`); what it shows belongs here, in place
of the "unverified" marks.

Sources relied on:

- the OpenAPI document, <https://developer.calendly.com/openapi/calendly-api.yaml>
  (`/users/me`, `/scheduled_events`, `/scheduled_events/{uuid}/invitees`,
  and the `User`, `Event`, `Invitee`, `Location`, `Cancellation`,
  `Guest` and `Pagination` schemas);
- the pages for [List Events](https://developer.calendly.com/api-docs/calendly-api/scheduled-events/list-scheduled-events.md),
  [List Event Invitees](https://developer.calendly.com/api-docs/calendly-api/scheduled-events/list-event-invitees.md),
  [Get current user](https://developer.calendly.com/api-docs/calendly-api/users/get-current-user.md),
  [rate limits](https://developer.calendly.com/api-docs/overview/rate-limits.md)
  and [personal access tokens](https://developer.calendly.com/docs/authentication/how-to-authenticate-with-personal-access-tokens.md).

**Sign-in.** latchkey's built-in `calendly` service holds a personal
access token (`latchkey auth set calendly -H "Authorization: Bearer
<token>"`); it has no browser login, so the wizard offers only the
paste. The reference names the scopes the three calls need:
`users:read` and `scheduled_events:read`. A `401` or `403` ends the
run and says how to store a token.

**What one run does.**

1. `GET /users/me`. Its `resource.uri` is the user; the last segment
   of it is the one calendar's id, and every event's key is
   `{user uuid}#{event uuid}`. The account row is `calendly`, its
   `login` the user's `email`, its `principal_href` the user URI (which
   render uses to name the signed-in user as organizer). A token for a
   different user drops the other user's calendar and meetings.
2. `GET /scheduled_events?user=<uri>&count=100&sort=start_time:asc`,
   with `min_start_time` / `max_start_time` for `since` / `until`, page
   after page by `page_token` (`pagination.next_page_token`). There is
   no `status` filter, so canceled meetings are listed too
   (unverified: the reference describes `status` as an optional filter
   and does not say what leaving it out returns). Each page is one
   transaction. The listing is whole every run: Calendly offers no
   delta token and no "updated since" filter, so there is nothing to
   resume from. When the last page lands and every meeting listed had
   a `uri`, the meetings the store holds that it did not name are
   dropped with their invitees. A page that fails, a reply with no
   `collection`, or a meeting with no `uri` drops nothing and leaves a
   `listing:meetings` row, until a later listing completes.
3. Each meeting's invitees are owed when `calendly_invitees` does not
   hold them at the meeting's current `listing_hash`:
   `GET /scheduled_events/{uuid}/invitees?count=100&sort=created_at:asc`,
   every page, through `datalib_etl_web::owed`, one meeting at a time.
   A meeting whose listing entry has not moved costs no request. One
   that fails stays owed with its error on the record; ten failures in
   a row end the loop for the run.

So a second run with nothing new costs `1 + ⌈meetings / 100⌉`
requests. Rate limits are 500 requests a minute per user on a paid
plan and 50 on the free one; a `429` is waited out for the
`X-RateLimit-Reset` seconds Calendly sends (the reference names that
header, not `Retry-After`).

**Unverified, and worth checking on the first live run:**

- That a change to who booked shows in the meeting's listing entry.
  Invitees are only read again when the entry's hash moves; the
  reference's `invitees_counter` (`total`, `active`, `limit`) and
  `updated_at` suggest a new or canceled invitee moves it, but an
  invitee's own edits (a no-show mark, a reconfirmation) may not, and
  would then not be picked up until the meeting changes.
- That `page_token` on a URL we build pages the same as following
  `pagination.next_page`, and that `next_page_token` is `null` on the
  last page.
- That an event's `location.join_url` stays put. The reference says
  conference details are dropped six months after a meeting; when that
  happens the stored copy loses its link too, since the listing is
  stored as sent.
- The order of `event_memberships` and `event_guests`. We sort both, so
  a reshuffle is not a change; if Calendly's own order meant something
  (the first host as organizer), it is lost, and render names the
  signed-in user as organizer instead.
- `start_time`'s precision (`2020-01-02T03:04:05.678123Z` in the
  reference) and that it is always UTC.

**Inspecting the result.** Stock `sqlite3` cannot open the store;
`datalib-doltlite` can (or `.dump` it into plain SQLite first):

```sh
db=<root>/<source id>/ingest/entities.doltlite_db
datalib-doltlite -readonly $db "SELECT id, start_time, status FROM calendly_events ORDER BY start_time;"
# Meetings whose invitees are owed: never fetched, or fetched for an older listing.
datalib-doltlite -readonly $db "SELECT e.id FROM calendly_events e LEFT JOIN calendly_invitees_bookkeeping b ON b.id = e.id WHERE b.held_version IS NOT e.listing_hash OR b.fetched_at_utc IS NULL;"
# Every answer to a booking question.
datalib-doltlite -readonly $db "SELECT i.id, json_extract(q.value, '$.question'), json_extract(q.value, '$.answer') FROM calendly_invitees i, json_each(json_extract(i.payload, '$.invitees')) v, json_each(json_extract(v.value, '$.questions_and_answers')) q;"
```
