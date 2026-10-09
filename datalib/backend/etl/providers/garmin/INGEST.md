# Garmin Connect download

The ingest step of a `garmin` group mirrors one Garmin Connect account
into a doltlite raw store, over the same API the Garmin Connect phone
app uses (`connectapi.garmin.com`). There is no public Garmin API for
individuals; this one is what `garth`, `python-garminconnect` and
GarminDB all sit on. The endpoints and query parameters here are the
same facts those projects observed; no code was ported from any of
them. GarminDB is GPL-2.0 and nothing was taken from it beyond which
URLs exist.

```
<data_root>/<group>/ingest/entities.doltlite_db
  garmin_account           the account's singletons, one row each
  garmin_devices           one row per registered device
  garmin_daily             one row per (metric, calendar day)
  garmin_weigh_ins         one row per weigh-in
  garmin_activities        one row per activity, as the listing shows it
  garmin_activity_details  the fuller per-activity record
  garmin_activity_files    edge to an activity's original FIT file in the CAS
  garmin_wellness_files    one row per day asked: its wellness FIT bundle in the CAS, or none (opt-in)
  garmin_items             personal records, gear, badges, workouts, goals
  coverage                 the start dates the activity listing has covered
<data_root>/<group>/ingest/blobs.sqlite
  cas_objects              the FIT files and bundles, keyed by blake3
```

## Where the data comes from, and what auth it needs

Every request goes through `latchkey curl`, like any other latchkey
source. The `garmin` service is not built into latchkey: it comes from
[latchkey-garmin](https://github.com/imbue-ai/latchkey-garmin), a
plugin datalib vendors and installs on the first Garmin sign-in
([`docs/dev/latchkey.md`](../../../../../docs/dev/latchkey.md#three-kinds-of-service)).

The plugin stores the year-long OAuth1 token and mints the hourly
bearer from it with an OAuth1-signed exchange whenever latchkey finds
the bearer expired, so the provider never sees either. Two ways in:

```sh
latchkey auth browser garmin                # Garmin's own sign-in page
latchkey auth set-nocurl garmin ~/.garth    # import a token garth wrote
```

When the OAuth1 token itself expires the exchange fails, the request
answers 401, and the run fails with the auth hint; sign in again.

## What one run does

Six phases, in this order. Every date walk is bounded below by
`api.since` (default: a year before the first run, recorded as
`garmin:default_since` so later runs start there too) and above by
`api.until` or the run's local date, whichever is earlier; a past
`until` fixes the window, so a mirror of a finished stretch stops
growing.

**No walk keeps a cursor.** What a run fetches is what the window lists
less what the store already holds, worked out from the store each run
([`data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md#what-is-left-to-fetch-listed-minus-held)). The shared form is
`datalib_etl_web::owed`: a listing is a key and a version per record; what
a row is held at is `held_version` in its table's `_bookkeeping`
sidecar, written in the transaction that stores the row; a record is
owed while the two differ, or while no fetch has landed. Each kind
below is one `owed::drain` over a fetcher of its own, one request per
record; days are written 31 to a transaction, details, files and
bundles 20, and a stop writes what was answered before it.

| what | listed | at version | held when | so a run fetches |
| --- | --- | --- | --- | --- |
| a day of a metric | every date of the window, per metric in `api.metrics` | the date the day settles — the day plus `api.refresh_days` (default 7; never less than one day) — or today while it has not | `garmin_daily` holds the row, an empty answer included, at that date | every day not yet settled, and settled days not held at their settle date |
| a wellness day | every date of the window | the same | `garmin_wellness_files` holds the row, with a bundle or without, at that date | the same |
| weigh-ins | the window | — | — | the whole window, every run |
| the activity listing | the window's start dates | — | a `coverage` span of scope `activities` covers them | from the lowest date not covered, or from `refresh_days` before the window's end if that is earlier |
| an activity's detail | each stored activity | its `listing_hash` | `garmin_activity_details` holds the row at it | details of activities that are new or whose listing entry changed |
| an activity's file | each stored activity whose file row has no bytes, when `activity_files` is on | its `listing_hash` | `garmin_activity_files` holds the row at it | files never answered for, and answers with no bytes whose activity has changed since |

A request that fails writes nothing but an attempt and the error in
the sidecar (for the Manage screen; nothing reads them back to decide
what to fetch), so the record is simply still owed. Because the owed
set is computed, a change of config needs no record: an earlier
`since` leaves days with no row and start dates with no span, and
turning `activity_files` on leaves every stored activity without a
file row.

Every prune below has the same gate, stated once here: **a walk deletes
what its listing did not name only when the listing was an
enumeration** — an array (every page of it, for the paged ones), with
no request failing. A 204/404/empty body, an object with no array
inside, or a page that errored is byte-similar to "everything was
deleted" and is not read that way: the stored rows stay, nothing is
recorded as covered, and the run records a `problems` row keyed
`listing:<name>` (see "What a failure leaves" below).

1. **Account and devices.** `/userprofile-service/socialProfile` (the
   `displayName` every per-user path needs) and `user-settings` land in
   `garmin_account`; `/device-service/deviceregistration/devices` in
   `garmin_devices`, pruned to the listing when it is one. The run
   cannot start without the social profile; a `user-settings` that
   fails leaves the stored row and a `listing:user_settings` row.
2. **Per-day metrics.** For each metric in `api.metrics` (default: all
   twenty in `DAILY_METRICS` in `garmin_config`), one request per owed
   calendar day, oldest first, stored verbatim in `garmin_daily` keyed
   `<metric>#<date>`. A day the endpoint had nothing for (204, 404,
   `{}` or `[]`) is stored as JSON `null`, so "asked, empty" is
   distinguishable from "never asked". Days are written a month (31
   days) at a time, so a run that dies re-fetches at most a month; a
   run that is stopped keeps the days answered before the stop. A day
   that failed has no row, so the next run asks again however old it
   is; once it lies outside the window its `problems` row goes
   instead.
3. **Weigh-ins.** `/weight-service/weight/range/<start>/<end>?includeAll=true`
   over the whole window in 90-day chunks, flattened one
   row per `samplePk` into `garmin_weigh_ins`. Rows dated inside the
   window that the listing did not name are deleted once every
   chunk has answered with a `dailyWeightSummaries` array: only then
   was the window listed completely, and their absence a deletion. A
   chunk that did not is skipped, the others still land, and nothing
   is pruned.
4. **Activities.** `/activitylist-service/activities/search/activities`
   paged from `startDate=<start>`, one row per `activityId` with the
   blake3 of its listing entry as `listing_hash`. The endpoint takes a
   start and lists to the present, so one walk from the lowest
   uncovered date covers every gap above it. When the page walk
   reaches a short page, one transaction stores the rows, deletes the
   activities dated a full day inside the listed stretch that it did
   not name (with their details and file rows), and records the
   stretch in `coverage`. A walk that stopped on a bad page still
   stores the pages it got, prunes nothing and covers nothing, so the
   next run lists the stretch again.

   Then every stored activity — listed this run or not — gets what it
   is owed: every detail, then every file. Its detail
   (`/activity-service/activity/<id>`) is held at the `listing_hash`
   it was fetched for; an activity Garmin has no detail for holds JSON
   `null` for that hash. Its file
   (`/download-service/files/activity/<id>`, `activity_files = false`
   turns it off) is the one `.fit` inside the zip, in the CAS. A
   download that answers 404 (an activity entered by hand) or holds no
   readable FIT is a file row with no bytes, held at the `listing_hash`
   it was answered for, so it is asked for again only once the
   activity's listing entry changes; the unreadable one is also a
   warning in `problems` until then (the fetch landed; what it held
   was lost). A request that failed holds nothing, and is asked again
   next run.
5. **Wellness FIT bundles** (`wellness_files = true`, default off).
   `/download-service/files/wellness/<date>` per owed day, the zip
   stored as-is: all-day heart rate, stress, steps, body battery and
   sleep at sensor resolution. Off by default because it is one zip
   per day and the per-day JSON already carries the same series at
   chart resolution. A day Garmin has no bundle for (404) is a row
   with no bytes, so it is not asked for again once it has settled; a
   bundle fetched while its day could still change is fetched again,
   like a day of a metric.
6. **Whole-account listings.** Personal records, gear, earned badges,
   workouts and active goals, each re-read complete every run into
   `garmin_items` (keyed `<kind>#<upstream id>`) and pruned to the
   listing when it is one. Workouts and goals are paged
   (`start`/`limit`, `ITEM_PAGE` at a time) to the first short page;
   the other three answer the whole list in one response. Gear is
   skipped when the social profile carries no `profileId`, and says
   so as `listing:gear`.

### What a failure leaves

A phase that fails wholesale is logged, counted in `errors=` and
`phases_failed=`, and the others still run; an auth failure aborts the
run, since every phase would share it. The run still exits 0 after a
failed phase, on purpose: the step driver commits the store and reports
its problem counts only when the run returns `Ok`, so failing the run
would hide the rows that say what failed. Every way a run falls short
is a row in the raw store's `problems` table, which the render step
carries downstream and the Manage screen counts:

| what | key | when it clears |
| --- | --- | --- |
| a day's metric that could not be fetched | `garmin_daily:<metric>#<date>` | a later run fetches the day, or the window no longer includes it |
| an activity detail, FIT file or wellness bundle that could not be fetched, or (a warning) a FIT that could not be read | `garmin_activity_details:<id>`, `garmin_activity_files:<id>#fit`, `garmin_wellness_files:<date>#wellness_zip` | a later run fetches it (or a file answers 404), or the activity is pruned |
| a listing that was not an enumeration, or could not be asked for | `listing:<user_settings\|devices\|weight\|activities\|personal_records\|gear\|badges\|workouts\|goals>` | the next run in which it lists |
| a phase that failed wholesale | `phase:<devices\|daily\|weight\|activities\|wellness\|items>` | the next run in which it runs |

The `listing:` and `phase:` rows of a run that reached its end replace
the last run's (`datalib_etl::run_problems`), so a listing that answers
again clears its row without anyone doing anything; a run that was
stopped or ended on an auth failure adds what it found and clears
none; a row that persists keeps its `first_seen_at_utc`. A `warn!` alone is never the
only record.

Each file edge carries its own record's hash. An earlier build keyed
every file of a batch under one ref, so each edge of a batch got the
last file's hash and a failure was stamped on all of them. Once per
table, on the first run with that table's walk turned on, an edge whose
hash another record's edge shares loses the hash and holds nothing, so
the walks above fetch it again; `sync_scope_state` records that it ran
(`garmin:shared_hash_repair:<table>`). Once only, because two
activities may share a file for real, and fetching those every run
would get the same bytes back.

Inside the per-day walk, a metric that fails ten days in a row is
abandoned for the run rather than paid for once per day of history: a
`phase:daily` row says so, the other metrics still run, and the run
counts as cut short (its `listing:` and `phase:` rows add to the last
run's rather than replacing them). The days it did not reach have no
row, so the next run asks for them.

A refused bearer ends the run, and so does a request latchkey will not
send at all (no credential, or the plugin's hourly token exchange
failing mid-run; latchkey exits 1 for both): every later request would
fail the same way.

### What a second run costs

Garmin has no "what changed since" API, so incrementality is the
trailing window: with the defaults, a second run the same day
re-fetches the seven days of every metric that have not settled
(20 × 7 = 140 requests), the weight window (one request per 90 days of
it), one activity page, and the five listings. Everything it re-fetches
is UPSERTed on its upstream id, so `dolt_diff` shows only real change.
The store commits once at the end of the run (and at checkpoints).

An earlier `api.since` costs the days and the activity start dates it
adds, once: they are the ones with no row and no span. A later one
changes nothing already stored.

### Interrupted runs

`tests/garmin_tests/interrupt.rs` cuts a replayed run off at one
request after another, two ways (the process dies; the step is told to
stop), commits whatever the store holds, runs again, and requires the
store an uninterrupted run leaves, the held versions included — once
from an empty store and once from a store an earlier run filled,
against an upstream that has changed since.

### What makes a record look changed

No field is declared volatile. Two runs a minute apart over the same
days changed no `garmin_daily` row (only its bookkeeping sidecar), so
the per-day payloads carry no per-fetch stamp. A device row's
`lastSyncTimestampGMT` moves as the watch syncs; whether anything else
churns has not been measured on an account with a watch.

## What the provider deliberately does not do

- It reads nothing but the account's own data; nothing is ever written
  to Garmin.
- No menstrual-cycle, pregnancy, nutrition, lifestyle-logging or golf
  endpoints, and no per-activity splits, weather or zone breakdowns
  beyond what the detail record carries. Each is one more entry in
  `DAILY_METRICS` or `ITEM_KINDS` when somebody wants it.
- Only the weigh-ins and devices are rendered; see
  [`../garmin_render/TRANSLATE.md`](../garmin_render/TRANSLATE.md).
- `garmin.cn` accounts are reachable (`login --domain garmin.cn`, and
  the token records its domain) but untested.

## Known gaps

- **Request volume on the first run.** Twenty metrics × every day since
  `since` is ~7,300 requests a year; three years of history is a
  long first sync. Trim `api.metrics` or start with a nearer `since`
  and widen it later. Garmin's rate limits are not documented; the
  shared transport backs off on 429 and gives up on the usual budget.
- **The workout and goal paging is verified only in playback.** Both
  endpoints take `start`/`limit` (the reference clients page them
  that way), but whether `start` is a 0-based offset, and what a page
  past the end answers, has not been watched against a live account
  with more than `ITEM_PAGE` of either. A wrong reading fails safe:
  an endpoint that ignores `start` answers the same page twice, the
  walk stops there as not an enumeration, and prunes nothing.
- **The playback fixtures assume the default `refresh_days`.** The
  synthesizer writes an activity-listing fixture for the two starts a
  listing takes with `refresh_days = 7` (the window's start, and a week
  before its end); a playback run that lists from another date misses.
- **One listing from the lowest gap.** The activity listing is asked
  from a start date only, so an earlier `since` re-lists everything
  from the new start, not only the stretch that was added. It costs
  listing pages, not details or files.
- **A store from an earlier build** opens on the migration ladder
  (`schema_raw::LADDER`). One that kept the held version in a column of
  the row (`fetched_on`, `listing_hash`) has it carried into the
  sidecar as it was: a detail or file at its listing version is not
  fetched again; a day fetched on the day it settled is not either,
  and one fetched later is fetched once more, since the rung cannot
  know `refresh_days`. A FIT the earlier build found unreadable is
  fetched once more too: that build stamped no fetch on it, and held
  now means a fetch landed. One from before those columns holds
  nothing, and the next run fetches every day of the window and every
  detail again, once.
- **Verified against one live account, but a thin one.**
  Every endpoint answered with the shape the reference code predicts,
  and the weigh-in columns were checked against real manual entries
  (`samplePk`, `weight` in grams, `timestampGMT`, `sourceType`). That
  account had no device paired, so the activity listing, the detail
  record, the FIT download and the device fields were exercised only
  through playback; check `start_time_gmt` and `activityType.typeKey`
  against the first real activity. What the probe did show about
  empty days: `hrv`, `endurance_score` and `hill_score` answer 204;
  `steps_chart`, `training_readiness`, `max_metrics`, `daily_events`
  and `body_battery_events` answer `[]`; `fitness_age` answers a 200
  with `{"invalidReason": …}` (stored as-is, since it is an answer);
  the wellness bundle answers 404. The rest answer a full object with
  null fields, which is also stored as-is.

## How to inspect the result

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=<data_root>/garmin/ingest/entities.doltlite_db

$dl -readonly $db "SELECT metric, COUNT(*), SUM(json(payload) <> 'null') FROM garmin_daily GROUP BY metric;"
$dl -readonly $db "SELECT calendar_date, weight_g/1000.0 AS kg, source_type FROM garmin_weigh_ins ORDER BY timestamp_gmt DESC LIMIT 10;"
$dl -readonly $db "SELECT activity_type, COUNT(*) FROM garmin_activities GROUP BY 1;"
$dl -readonly $db "SELECT scope, lo, hi FROM coverage;"
$dl -readonly $db "SELECT d.metric, MIN(d.calendar_date), MAX(d.calendar_date), MAX(b.held_version) FROM garmin_daily d JOIN garmin_daily_bookkeeping b ON b.id = d.id GROUP BY d.metric;"
$dl -readonly $db "SELECT a.id FROM garmin_activities a LEFT JOIN garmin_activity_details_bookkeeping d ON d.id = a.id WHERE d.held_version IS NOT a.listing_hash;"
$dl -readonly $db "SELECT json_extract(json(payload), '$.sleepScores.overall.value') FROM garmin_daily WHERE metric = 'sleep' AND calendar_date = '2026-09-13';"
```

Stock `sqlite3` cannot open the file;
[`docs/dev/doltlite.md`](/docs/dev/doltlite.md#getting-the-data-out-export-to-plain-sqlite)
has the one-pipe export.
