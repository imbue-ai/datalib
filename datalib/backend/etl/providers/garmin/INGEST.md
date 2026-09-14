# Garmin Connect download

The ingest step of a `garmin` group mirrors one Garmin Connect account
into a doltlite raw store, over the same API the Garmin Connect phone
app uses (`connectapi.garmin.com`). There is no public Garmin API for
individuals; this one is what `garth`, `python-garminconnect` and
GarminDB all sit on, and the request shapes here are ported from them.

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

1. **Account and devices.** `/userprofile-service/socialProfile` (the
   `displayName` every per-user path needs) and `user-settings` land in
   `garmin_account`; `/device-service/deviceregistration/devices` in
   `garmin_devices`, pruned to the listing.
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
   walked window that the listing did not name are deleted: the
   window was listed completely, so their absence is a deletion.
4. **Activities.** `/activitylist-service/activities/search/activities`
   paged from `startDate=<cursor − refresh_days>`, one row per
   `activityId`. An activity whose listing row is new or whose payload
   differs from the stored one gets its detail
   (`/activity-service/activity/<id>`) re-fetched; one with no FIT file
   in the CAS yet gets `/download-service/files/activity/<id>` fetched,
   the one `.fit` inside the zip stored, and an edge row written
   (`activity_files = false` turns that off). Activities dated a full
   day inside the walked window that the listing did not name are
   pruned with their details and edges.
5. **Wellness FIT bundles** (`wellness_files = true`, default off).
   `/download-service/files/wellness/<date>` per day, the zip stored
   as-is: all-day heart rate, stress, steps, body battery and sleep at
   sensor resolution. Off by default because it is one zip per day and
   the per-day JSON already carries the same series at chart
   resolution. A bundle already stored is never re-fetched.
6. **Whole-account listings.** Personal records, gear, earned badges,
   workouts and active goals, each re-read complete every run into
   `garmin_items` (keyed `<kind>#<upstream id>`) and pruned to the
   listing.

A phase that fails is logged and counted in `errors=`, and the others
still run; an auth failure aborts the run, since every phase would
share it. Inside the per-day walk, a metric that fails ten days in a
row is abandoned for the run rather than paid for once per day of
history; the failed days carry the error in their bookkeeping row and
are re-tried inside the next run's refresh window.

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

Nothing has been declared volatile yet. Garmin's replies do carry
per-fetch fields in places — a `lastSyncTimestampGMT` on a device, and
almost certainly stamps inside some daily payloads — and those will
churn `dolt_diff` inside the refresh window. Measure it against a real
store before declaring anything: the etl README's rule is that a
volatile field carries no information, and a field that only *looks*
like noise is not one.

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
- **The playback fixtures assume the default `refresh_days`.** The
  synthesizer writes the weight and activity fixtures for the windows a
  first run and a second run with `refresh_days = 7` ask for; a
  playback run with another value misses.
- **Endpoint shapes are as the reference implementations show them**,
  not verified against a live account by this provider yet — the
  live probe is the next thing to do (see the `verify-api-shape` rule
  in AGENTS.md), and the promoted columns (`start_time_gmt`,
  `weight_g`, …) are the ones to check first.

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
