# AirVisual (IQAir) air-quality monitors as a source

**Status: investigation (2026-09-14), checked against the tree on
2026-11-27. Route C, the Pro's own files, is built; Route A, the device
API, and Route B, the dashboard export, are not.** The `airvisual`
provider reads an AirVisual Pro's history files off its Samba share
into doltlite, and renders them as documents and as time series
([`providers/airvisual/INGEST.md`](../../../datalib/backend/etl/providers/airvisual/INGEST.md)
is the reference). What follows is kept for the measurements of the
cloud routes, which are the reason the device API (`api`) is still
worth building for the *published* outdoor unit and nothing else. Every claim below about
IQAir's endpoints was measured on 2026-09-14: first against two
*public* stations in Nakuru, Kenya (ids `6856994e52fb712759ff48a7` and
`62b9caaf8e60f0ce6659e0c6`, taken from a public GitHub pipeline that
reads them), then against the three devices on a real free-plan
dashboard account — one published AirVisual Outdoor and two private
AirVisual Pros (§"What a real account has"). Claims marked
*unverified* come from other people's code or IQAir's help pages and
were not exercised here.

## The answer in one paragraph

Yes. An IQAir monitor that is registered in the IQAir dashboard has a
**device API**, `GET https://device.iqair.com/v2/<device id>`, that
needs no credential and returns the device's current reading plus four
trailing windows of history. An `api` method that fetches that once per
run and upserts every sample it carries into the store Route C already
fills is the `yolink` design with the window-walking removed. The one
thing that is *worse* than yolink: the endpoint takes no
date range, so there is no backfill and no re-walk. **The windows are
the retention.** A minute-resolution station keeps one hour of
minute-level samples; miss an hour and that hour is hourly averages
forever, miss two days and it is daily averages. Deeper history exists
only behind the dashboard login (§"Route B") or on the device's own
disk (§"Route C", Pro units only).

## What the device API returns

Measured, both stations, 2026-09-14. The shape differs by device
generation and the parser has to take both.

| | station A (`NAKURU MET STATION`, an AirVisual Outdoor) | station B (`Nakuru county env. office`, an older Node/Pro-class unit) |
|---|---|---|
| top-level keys | `name`, `current`, `historical` | `settings.node_name`, `current`, `historical`, `subscription`, `isPublic`, `formattedMeasurement` |
| `historical.instant` | 60 samples, **60 s apart** → the last 59 min | 60 samples, **900 s apart** → the last ~15 h |
| `historical.hourly` | 48 buckets | 96 buckets |
| `historical.daily` | 30 | 30 |
| `historical.monthly` | 12 | 12 |
| sample shape | `{ts, co2, pm1, pr, hm, tp, pm25:{conc,aqius,aqicn}, pm10:{…}}` | instant: `{ts, tp, hm, p01, p2, p1, co2, errors:{voc:-202,hcho:-205}}`; hourly/daily/monthly: `{ts, p2_sum, p2_count, p1_sum, p1_count, p01_sum, …, co2_sum, co2_count, tp_sum, …, hm_sum, …}` |

So `instant` is *the last 60 samples*, not the last hour; how much
wall-clock that covers depends on the device's reporting interval. The
`_sum`/`_count` pairs on the older generation are what the newer one
has already divided out. `errors.voc = -202` is a sensor-not-fitted
sentinel, not a reading.

Station B's newest sample is 2026-07-08 — it has been offline for two
months and the endpoint still answers `200` with a full, stale
`historical`. That is the same blind spot `yolink/INGEST.md` documents
("`errors=0` does not mean healthy"), and the same `MAX(ts)`-per-device
check is the answer.

**The endpoint ignores every range parameter.** `?from=…&to=…`,
`?start=<ms>&end=<ms>`, `?period=daily&limit=365` all return the
byte-identical 24,990-byte document; `/history`, `/measurements`,
`/historical`, `/export` under the id are `404`. There is no way to ask
it for last week.

The id is a 24-hex Mongo-style id, *not* the device's serial number and
not its share code. The dashboard shows it as the "device API" link at
the bottom of a device's detail page (IQAir's KB "Download or export
your AirVisual device's data"); station B's `subscription.device_api:
true` is the flag that link exists. For a **private** device the
device API is a dashboard-subscription feature; for a device published
as a public outdoor station it is free. Whether an unpublished device's
URL answers without a login was not tested — neither Nakuru station is
ours.

## What a real account has

Three devices on one free-plan dashboard login, read off the
dashboard on 2026-09-14 (device ids deliberately not written down here):

| device | model | published? | device API | export |
|---|---|---|---|---|
| outdoor unit | AirVisual Outdoor | yes, as a public station | **works**, no credential: `instant` = 60 samples at 60 s = **one hour**, `hourly` 48, `daily` 30, `monthly` 12; same shape as station A above. Plus a second link, `…/validated-data`, the public *station's* hourly view (43 buckets, with weather) | offered |
| indoor unit 1 | AirVisual Pro | no (private) | **padlocked** — the link is masked `https://dev-public-api.airvisual.net/devices/***` with a lock icon; neither of the ids visible in its dashboard URL answers on `device.iqair.com` | **"Only data from public devices can be exported on your current plan."** |
| indoor unit 2 | AirVisual Pro, offline since 2026-09-13 | no (private) | padlocked, as above | same |

The plan comparison behind the dashboard's "Upgrade" button settles
what a private device gets on the free plan: real-time readings and
the hourly/daily/monthly charts, and nothing programmatic. "My
device's API", "Download historical raw data of devices (every
measurement this monitor has taken)" and the validated-data download
are each starred "available for the contributors who own public
stations", which in practice means *for the public devices* — the
padlock on the private Pro sits on an account that does own a public
station. The paid tier is "Get in touch", a sales conversation rather
than a price, so it is not a route.

So for this fleet the routes divide by device, not by preference:

- **The outdoor unit is Route A**, today, for free — and is also
  the one device the dashboard *will* export, so its minute-level
  history past the one-hour window is reachable, once, as a CSV.
- **The two Pros are Route C** — the device's own disk over Samba —
  which is also the only route that gives every sample the monitor
  ever took, for any device. The cloud routes for a private Pro are
  closed unless it is published as an "indoor sensor" (the dashboard
  has that under *Publications*), which is a privacy decision: a
  bedroom's CO₂ and PM2.5 series is an occupancy log.

## The three routes, and which to build

### Route A — device API (build this first)

No credential, one request per device per run, JSON. Everything above.
A method called `api`, reaching a service, like yolink's.

The cost is the retention window, and the answer to it is cadence.
Polled often enough, this route yields **complete** minute-level data,
because every sample the device reports sits in the `instant` window
for 60 reporting intervals and an upsert of an already-stored sample is
a no-op. For the real outdoor unit (one sample a minute):

| fetch at least every | you keep, permanently |
|---|---|
| ~50 min (60 samples, with margin) | every minute-level sample |
| ~40 h (48 hourly buckets) | hourly averages |
| ~28 days (30 daily buckets) | daily averages |

A 15-minute cadence is 96 requests of ~23 KB a day per device, no
credential, and leaves a 45-minute margin for a laptop asleep or a
sync that ran long. No rate limit was met at the handful of requests
made here; none is documented; a `429` should be treated as the
failure it is rather than retried into. (A 15-minute-interval Pro, if
its device API were open, would tolerate 15 hours between fetches.)

That is not a reason not to build it — every fetched sample is in the
mirror for good, which is the whole point — but it means the source is
only as good as the sync schedule, and the Manage screen should say so
rather than let a quiet lapse look like a quiet sensor. A `last_seen`
column per device and per tier, compared against now, is the cheapest
honest signal.

**Nothing in Datalib runs a sync on a clock** (still true on
2026-11-27). A sync starts when someone opens a request: the Manage
screen's Sync, `datalib-pipeline sync`, or `POST /api/requests` against
a running `datalib-http` (`datalib/backend/dag/README.md` §"What the
loop runs"). So "fetch every 15 minutes" is a `launchd`/`cron` entry
doing one of those, and the app itself never keeps the promise. yolink
has lived with the same gap. A periodic sync — a per-group interval
the http worker honours while the app is open — is the feature this
source makes worth building, and it is out of scope here beyond
naming it.

### Route B — dashboard export (deep history; later, and conditional)

The dashboard (`dashboard.iqair.com`, an Angular app whose bundles are
public) talks to `https://website-api.airvisual.com/v2/` with an
`x-login-token` header, obtained by `POST /v2/auth/signin/by/email`
with `{email, password}` — that call returns `{id, email, name,
loginToken}`, and `id` is the `{user_id}` the rest of the API is keyed
on. Read out of the bundle (`chunks/1924.*.js`, the "downloads"
service), the export is an **async job**:

```
POST /v2/users/{user_id}/downloads
     {startDate, endDate,               ISO-8601 UTC
      interval: minutely|hourly|daily|monthly,
      type: raw|validated,
      devices: [<device id>, …]}       or deviceGroups: […]
GET  /v2/users/{user_id}/downloads/{id}   poll state.label: Pending|Ongoing|Ready|Failed
     → file.link (expiring), file.expiresAt
GET  /v2/users/{user_id}/devices?page=1&perPage=15   the device list, with current readings
```

The form's presets go back "last 12 months" and allow a custom range,
so this is the arbitrary-range history that Route A lacks, at minute
resolution. All of it is *unverified* here — no login was attempted —
and three things gate it:

1. **Credential.** The token is not a cookie; the app keeps it in
   local storage. latchkey can carry it as an injected header on a
   custom service (`latchkey auth set <svc> -H "x-login-token: …"`, the
   way the Notion provider takes `Authorization: Bearer`), but the
   browser-login capture flow would have to be checked against a
   non-cookie token. Typing the password into anything of ours is not
   on the table.
2. **Entitlement.** IQAir's own KB says data export is free for
   *published* outdoor devices and needs a Dashboard subscription for
   private ones. The permission enum in the bundle
   (`downloadHistoricalRaw`, `downloadHistoricalAggregated`,
   `downloadValidatedData`) is per plan.
3. **Shape.** The CSV columns were not seen. IQAir's KB describes the
   export as "raw measurements and aggregated hourly data".

On the account above this is available for the public outdoor unit
only, and gate 2 is the free plan's own wording. That changes what is
worth building: not an automated `dashboard` method with a captured
login token, but a **`Local` `export` method that reads the CSV the
dashboard hands you** — run the export by hand in the browser once (or
once a season), point the step at the file, and let the device API
keep the tip fresh. That is the "seed from an export, keep fresh from
the API" pattern the Claude provider has, with the same caveat that the
two must agree on ids and timestamps, and it costs no credential
handling at all. The CSV's columns have not been seen yet; one export
of the outdoor unit would show them.

### Route C — the Pro's Samba share: built

An AirVisual **Pro** serves `smb://<ip>/airvisual` with one file per
month of every sample it took. The `export` method reads that folder,
mounted or copied; `airvisual/INGEST.md` has what the share holds, the
device's clock, incrementality and the measurements. Outdoor units
have no share, so for the two Pros on the real account it is the only
route.

### Not routes

- **`api.airvisual.com/v2` (the documented AirVisual API).** City- and
  station-level public data for a key; the free tier is city-level
  only. It is not a way at your own device's history and is not
  proposed here.
- **`app-api.airvisual.com/api/v5/devices/{id}/measurements`** with a
  token hard-coded in the mobile app (a 2020 blog post). Unverified,
  possibly dead, and hourly only. Route B supersedes it.

## Design (Route A)

The first design here was written before Route C and assumed yolink's
shape throughout: a JSON-payload readings table keyed
`device#tier#ts#metric`, and a copy of `yolink_render`. Route C settled
the store and the render instead (`airvisual/INGEST.md`): typed rows
in `airvisual_samples` keyed `(device_id, ts_ms)`, and the shared
`datalib_etl_timeseries_render` crate. What still stands for an `api`
method:

**Naming.** The house rule is *the product a person recognises, never
the vendor*. The monitors are "AirVisual Outdoor" / "AirVisual Pro",
the app is "IQAir AirVisual", the API host is `device.iqair.com`. The
type is `airvisual`; `iqair` goes in the catalog keywords.

**Config.** One type, two methods. A step carries one method, so the
real fleet needs two groups:

```toml
[[groups]]
id = "air-outdoor"
type = "airvisual"
name = "Outdoor air"

[[steps]]
id = "air-outdoor/ingest"
command = "datalib-step run ingest airvisual"
[[steps.params.api.devices]]
name = "backyard"                      # row key; keep it stable
device_id = "0123456789abcdef01234567" # the 24-hex id from the dashboard's device-API link
```

No `start`, no `window_days`, no `overlap`: there is nothing to walk.
`device_id` grants read access to the trailing windows of one device,
which a public station already gives everyone, but it is the only thing
between a stranger and a private device's readings, so it stays out of
committed configs.

**Parse.** Both device generations map to one set of measurements:
`pm25.conc` and `p2` are both PM2.5; `p2_sum / p2_count` is the mean;
`aqius` / `aqicn` under `pm25` are the AQI on the same timestamp; a
sentinel in `errors` gives no value. How the coarser tiers sit beside
the instant samples in `airvisual_samples` is still to decide: an
hourly bucket and an instant sample can share a timestamp and are
different facts, so either the tier joins the key or only the instant
samples are kept, with coarser buckets filling the hours that have
none.

**The open buckets.** The newest hourly, daily and monthly bucket
changes on every fetch until its period closes, so the upsert
overwrites it and every run changes a few rows. That is correct; it
only means "rows changed" is never zero.

**Fetch.** Per device, one plain `curl` (`HttpRequest::plain`: no
credential, and `device.iqair.com` was not behind Cloudflare's
challenge), parse, one upsert, and the device's newest sample per tier
recorded (the honest-lapse signal above). Every run fetches the same
thing, so there is no cursor. A reset and resync cannot recover
anything older than the windows, so the run's summary should say so
rather than look like a backfill.

**Tests.** Capture one response from each generation as fixtures:
station B's stale `historical` is the dead-sensor case for free. A live
test against a public station needs no key, and would only guard
against IQAir changing the shape.

## Settled, and still open

Settled on 2026-09-14 from the real account: the fleet is one
published Outdoor plus two private Pros; the outdoor unit's device API
works and its export is offered; the Pros get neither on the free plan;
the paid plan is a sales contact. Route C is built and measured against
a real Pro (`INGEST.md`); the type is `airvisual`; nothing runs a sync
on a clock.

Still open:

1. **Route A, the `api` method**, for the published outdoor unit: one
   `curl` per run into the same `airvisual_samples` table, with the
   tiers folded in as §"Design (Route A)" says. Its cadence argument
   stands, and so does the missing periodic sync.
2. **The dashboard export's columns**, for the outdoor unit's one-time
   backfill. One export from the browser shows them; the job is created
   on the account, so it is the owner's to run.
3. **Publish the Pros as indoor sensors, or not.** Would open Route A
   for them; publishes a bedroom's occupancy pattern. Owner's call, and
   Route C does not need it.
