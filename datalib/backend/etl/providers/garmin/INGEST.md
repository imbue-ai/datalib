# Garmin Connect download

The ingest step of a `garmin` group mirrors one Garmin Connect account
into a doltlite raw store, over the same API the Garmin Connect phone
app uses (`connectapi.garmin.com`). There is no public Garmin API for
individuals; this one is what `garth`, `python-garminconnect` and
GarminDB all sit on. The endpoints and query parameters here are the
same facts those projects observed; the only code ported from any of
them is the SSO login, from `garth` (MIT). GarminDB is GPL-2.0 and
nothing was taken from it beyond which URLs exist.

```
<data_root>/<group>/ingest/entities.doltlite_db
  garmin_account           the account's singletons, one row each
  garmin_devices           one row per registered device
  garmin_daily             one row per (metric, calendar day)
  garmin_weigh_ins         one row per weigh-in
  garmin_activities        one row per activity, as the listing shows it
  garmin_activity_details  the fuller per-activity record
  garmin_activity_files    edge to an activity's original FIT file in the CAS
  garmin_wellness_files    edge to a day's wellness FIT bundle in the CAS (opt-in)
  garmin_items             personal records, gear, badges, workouts, goals
<data_root>/<group>/ingest/blobs.doltlite_db
  cas_objects              the FIT files and bundles, keyed by blake3
```

## Where the data comes from, and what auth it needs

**Garmin is not a latchkey service.** Every API call carries an OAuth2
bearer that expires in about an hour, and a fresh one is minted by an
OAuth1-*signed* request (HMAC-SHA1 over a nonce, a timestamp and a
stored token secret). Latchkey can inject a static header, or capture a
cookie or a token a web app mints, but it cannot sign; so the credential
lives with the provider, the way `yolink` signs its own download URLs.

The durable secret is the **OAuth1 token**, which lasts about a year.
A login produces it:

```sh
datalib-step login garmin            # prompts: email, password, the emailed MFA code
```

That writes `oauth1_token.json` and `oauth2_token.json` under
`api.token_dir` (default `~/.garth`), mode 600, in garth's own file
format — so a token produced by garth (`garth login`, then
`garth.client.dump("~/.garth")`) works unchanged, and vice versa. The
ingest step reads the OAuth1 token, refreshes the bearer when the
cached one has expired, and writes the fresh bearer back beside it.

The SSO login is the Connect app's flow (`sso.garmin.com/mobile/api/login`,
then `/mobile/api/mfa/verifyCode`, then a service ticket exchanged at
`connectapi.garmin.com/oauth-service/oauth/preauthorized`), ported from
garth's `sso.py`. The OAuth1 consumer it signs with is the Connect
Android app's, pinned in `src/auth.rs` rather than fetched from the S3
file garth reads it from. When the OAuth1 token itself expires the
bearer exchange answers 401 and the run fails with the auth hint; log
in again.

A proper `garmin` service in latchkey, computing the bearer per request
the way its `set-nocurl` services do, is the right long-term home for
this; it would move the login into the wizard's Connect button. Not
done here — it is JavaScript in another repo, then a release and a pin
bump.

## What one run does

Five walks, each with its own cursor in `sync_scope_state`, all bounded
below by `api.since` (default: a year before the first run) and above
by the run's local date:

Every prune below has the same gate, stated once here: **a walk deletes
what its listing did not name only when the listing was an
enumeration** — an array (every page of it, for the paged ones), with
no request failing. A 204/404/empty body, an object with no array
inside, or a page that errored is byte-similar to "everything was
deleted" and is not read that way: the stored rows stay, the cursor
stays so the next run walks the window again, and the run records a
`problems` row keyed `listing:<name>` (see "What a failure leaves"
below).

1. **Account and devices.** `/userprofile-service/socialProfile` (the
   `displayName` every per-user path needs) and `user-settings` land in
   `garmin_account`; `/device-service/deviceregistration/devices` in
   `garmin_devices`, pruned to the listing when it is one.
2. **Per-day metrics.** For each metric in `api.metrics` (default: all
   twenty in `DAILY_METRICS`), one request per calendar day from the
   metric's cursor less `refresh_days` (default 7) to today, stored
   verbatim in `garmin_daily` keyed `<metric>#<date>`. A day the
   endpoint had nothing for (204, 404, `{}` or `[]`) is stored as JSON
   `null`, so "asked, empty" is distinguishable from "never asked".
   The cursor advances per day, so a Ctrl-C loses nothing.
3. **Weigh-ins.** `/weight-service/weight/range/<start>/<end>?includeAll=true`
   in 90-day chunks from the cursor less `refresh_days`, flattened one
   row per `samplePk` into `garmin_weigh_ins`. Rows dated inside the
   walked window that the listing did not name are deleted once every
   chunk has answered with a `dailyWeightSummaries` array: only then
   was the window listed completely, and their absence a deletion. A
   chunk that did not is skipped, the others still land, and the
   window is neither pruned nor cursored past.
4. **Activities.** `/activitylist-service/activities/search/activities`
   paged from `startDate=<cursor − refresh_days>`, one row per
   `activityId`. An activity whose listing row is new or whose payload
   differs from the stored one gets its detail
   (`/activity-service/activity/<id>`) re-fetched; one with no FIT file
   in the CAS yet gets `/download-service/files/activity/<id>` fetched,
   the one `.fit` inside the zip stored, and an edge row written
   (`activity_files = false` turns that off). Activities dated a full
   day inside the walked window that the listing did not name are
   pruned with their details and edges — once the page walk reached a
   short page. A walk that stopped on a bad page still upserts the
   pages it got, and prunes nothing.
5. **Wellness FIT bundles** (`wellness_files = true`, default off).
   `/download-service/files/wellness/<date>` per day, the zip stored
   as-is: all-day heart rate, stress, steps, body battery and sleep at
   sensor resolution. Off by default because it is one zip per day and
   the per-day JSON already carries the same series at chart
   resolution. A bundle already stored is never re-fetched.
6. **Whole-account listings.** Personal records, gear, earned badges,
   workouts and active goals, each re-read complete every run into
   `garmin_items` (keyed `<kind>#<upstream id>`) and pruned to the
   listing when it is one. Workouts and goals are paged
   (`start`/`limit`, `ITEM_PAGE` at a time) to the first short page;
   the other three answer the whole list in one response.

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
| a day's metric that could not be fetched | `garmin_daily:<metric>#<date>` | the day fetches inside a later refresh window |
| an activity detail, FIT file or wellness bundle that could not be fetched | `garmin_activity_details:<id>`, `garmin_activity_files:<id>#fit`, `garmin_wellness_files:<date>#wellness_zip` | it fetches |
| a listing that was not an enumeration | `listing:<devices\|weight\|activities\|personal_records\|gear\|badges\|workouts\|goals>` | the next run in which it lists |
| a phase that failed wholesale | `phase:<devices\|daily\|weight\|activities\|wellness\|items>` | the next run in which it runs |

The `listing:` and `phase:` rows are replaced whole each run
(`datalib_etl::download_problems::report_run`), so a listing that
answers again clears its row without anyone doing anything; a row that
persists keeps its `first_seen_at_utc`. A `warn!` alone is never the
only record.

Inside the per-day walk, a metric that fails ten days in a row is
abandoned for the run rather than paid for once per day of history;
the failed days carry the error in their bookkeeping row and are
re-tried inside the next run's refresh window.

### What a second run costs

Garmin has no "what changed since" API, so incrementality is the
trailing window: with the defaults, a second run re-fetches the last
seven days of every metric (20 × 8 = 160 requests), one weight chunk,
one activity page, and the five listings. Everything it re-fetches is
UPSERTed on its upstream id, so `dolt_diff` shows only real change.
The store commits once at the end of the run (and at checkpoints).

Widening `api.since` re-walks every cursor from the new start; the
`since` each run was walked under is recorded in `sync_scope_config`
so the widening is detected rather than guessed from the cursors.
Narrowing it changes nothing already stored.

### What makes a record look changed

Nothing has been declared volatile yet. Measured on the live account
above: two runs a minute apart over the same five days changed no
`garmin_daily` row (`dolt_diff_stat` between the two run commits is
empty for the table and reports 100 modified rows for its bookkeeping
sidecar, one per re-fetched day), so the per-day payloads at least
carry no per-fetch stamp. A `lastSyncTimestampGMT` on a device
row will move as the watch syncs; whether anything else churns is
still to be measured against an account with a watch on it. The etl
README's rule applies: a volatile field carries no information, and a
field that only *looks* like noise is not one.

## What the provider deliberately does not do

- It reads nothing but the account's own data; nothing is ever written
  to Garmin.
- No menstrual-cycle, pregnancy, nutrition, lifestyle-logging or golf
  endpoints, and no per-activity splits, weather or zone breakdowns
  beyond what the detail record carries. Each is one more entry in
  `DAILY_METRICS` or `ITEM_KINDS` when somebody wants it.
- Only `weigh_ins` are rendered so far; see `../garmin_render/TRANSLATE.md`.
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
  synthesizer writes the weight and activity fixtures for the windows a
  first run and a second run with `refresh_days = 7` ask for; a
  playback run with another value misses.
- **Verified against one live account (2026-09-14), but a thin one.**
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

$dl $db "SELECT metric, COUNT(*), SUM(json(payload) <> 'null') FROM garmin_daily GROUP BY metric;"
$dl $db "SELECT calendar_date, weight_g/1000.0 AS kg, source_type FROM garmin_weigh_ins ORDER BY timestamp_gmt DESC LIMIT 10;"
$dl $db "SELECT activity_type, COUNT(*) FROM garmin_activities GROUP BY 1;"
$dl $db "SELECT scope, last_seen_at_utc FROM sync_scope_state WHERE scope LIKE 'garmin:%';"
$dl $db "SELECT json_extract(json(payload), '$.sleepScores.overall.value') FROM garmin_daily WHERE metric = 'sleep' AND calendar_date = '2026-09-13';"
```

Stock `sqlite3` cannot open the file; to get a plain SQLite copy,
`datalib-doltlite -readonly $db .dump | sqlite3 out.sqlite`.
