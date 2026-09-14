# AirVisual (IQAir) air-quality monitors as a source

**Status: investigation (2026-09-14). Nothing is built.** This is the
second member of the time-series family that
[`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
§"Examples where schema and data handling should be unified" lists as
"IQ Air air quality planned", beside `yolink`. Every claim below about
IQAir's endpoints was measured on 2026-09-14 against two *public*
stations in Nakuru, Kenya (ids `6856994e52fb712759ff48a7` and
`62b9caaf8e60f0ce6659e0c6`, taken from a public GitHub pipeline that
reads them); claims marked *unverified* come from other people's code or
IQAir's help pages and were not exercised here.

## The answer in one paragraph

Yes. An IQAir monitor that is registered in the IQAir dashboard has a
**device API**, `GET https://device.iqair.com/v2/<device id>`, that
needs no credential and returns the device's current reading plus four
trailing windows of history. An ingest step that fetches that once per
run and upserts every sample it carries into a doltlite store keyed
`device#tier#ts#metric` is the `yolink` design with the window-walking
removed, and the `yolink_render` page (one Plotly plot per physical
quantity, one series per device) renders it with a different metric
table. The one thing that is *worse* than yolink: the endpoint takes no
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

## The three routes, and which to build

### Route A — device API (build this first)

No credential, one request per device per run, JSON. Everything above.
An `Origin` method called `api`, like yolink's.

The cost is the retention window. To keep the minute-level tier of a
1-minute station you have to sync inside every hour; the hourly tier
gives you two days; daily, a month. That is not a reason not to build
it — every fetched sample is in the mirror for good, which is the whole
point — but it means the source is only as good as the sync schedule,
and the Manage screen should say so rather than let a quiet lapse look
like a quiet sensor. A `last_seen` column per device and per tier,
compared against now, is the cheapest honest signal.

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

Worth building only if the answer to §"Open questions" 2 and 3 is
yes. If it is, it slots in as a second `Origin` method on the same
group (`dashboard`), writing the same `airvisual_readings` table with
`tier = 'export'`, and the device API keeps the tip fresh between
exports — the same "seed from an export, keep fresh from the API"
pattern the Claude provider has, with the same caveat that the two must
agree on ids.

### Route C — the Pro's Samba share (deep history; only for a Pro)

An AirVisual **Pro** (the indoor unit with a screen) serves
`smb://<ip>/airvisual` (user `airvisual`, password on the device) with
one semicolon-separated file per month, `YYYYMM_AirVisual_values.txt`,
"up to 5 years" per IQAir. Columns, from Home Assistant's `pyairvisual`
fixture (*unverified* against a real unit):

```
Date;Time;Timestamp;PM2_5(ug/m3);AQI(US);AQI(CN);PM10(ug/m3);PM01(ug/m3);
Outdoor AQI(US);Outdoor AQI(CN);Temperature(C);Temperature(F);
Humidity(%RH);CO2(ppm);SGPCO2(ppm);VOC(ppb);SGPCO2LTC(ppm);VOCLTC(ppb)
```

This is a `Local` method (`export`, with a `path` to the mounted share
or to copied files), the shape `fsindex`/`pdf`/`media` already have.
Outdoor units have no share. Not worth designing further until we know
there is a Pro in the house.

### Not routes

- **`api.airvisual.com/v2` (the documented AirVisual API).** City- and
  station-level public data for a key; the free tier is city-level
  only. It is not a way at your own device's history and is not
  proposed here.
- **`app-api.airvisual.com/api/v5/devices/{id}/measurements`** with a
  token hard-coded in the mobile app (a 2020 blog post). Unverified,
  possibly dead, and hourly only. Route B supersedes it.

## Design (Route A)

Mirror yolink's crates: `airvisual_config`, `airvisual`,
`airvisual_render`.

**Naming.** The house rule is *the product a person recognises, never
the vendor* (`claude` not `anthropic`). The monitors are "AirVisual
Outdoor" / "AirVisual Pro", the app is "IQAir AirVisual", the API host
is `device.iqair.com`. `airvisual` is the product; `iqair` goes in the
catalog keywords the way `anthropic` does. Open to being overruled —
see §"Open questions" 5.

**Config.** One device list, as yolink:

```toml
[[groups]]
id = "airvisual"
type = "airvisual"
name = "Air quality"

[[steps]]
group = "airvisual"
function = "ingest"
[steps.params.api]
[[steps.params.api.devices]]
name = "backyard"                      # row key; keep it stable
device_id = "6856994e52fb712759ff48a7" # the 24-hex id from the dashboard's device-API link
```

No `start`, no `window_days`, no `overlap` — there is nothing to walk.
`device_id` is not a secret in the yolink sense (it grants read access
to the trailing windows of one device, which a public station already
gives everyone), but it is the only thing between a stranger and a
private device's readings, so keep it out of committed configs all the
same.

**Raw store.** `airvisual_devices` (`id` = name, `device_id`, plus
`last_ts_ms` per tier as four nullable columns — the honest-lapse
signal above) and `airvisual_readings` as a `WirePayloadRow`:

```
id           "{device}#{tier}#{ts_ms}#{metric}"
payload      the sample object as the wire had it, whole
device_name
tier         instant | hourly | daily | monthly
ts_ms
metric       pm25_ugm3 | pm10_ugm3 | pm1_ugm3 | co2_ppm | temperature_c
             | humidity_pct | pressure_pa | aqi_us | aqi_cn
value        f64
```

Normalising both generations to one metric vocabulary happens at
parse: `pm25.conc` and `p2` are both `pm25_ugm3`; `p2_sum/p2_count` is
the mean, with the raw pair still in `payload`; `aqius`/`aqicn` under
`pm25` become `aqi_us`/`aqi_cn` on the same ts; a sentinel in `errors`
emits no row. `tier` is in the key because an hourly bucket and an
instant sample can share a timestamp and are different facts.

The newest hourly/daily/monthly bucket is *open* — it changes on every
fetch until the period closes — so the upsert overwrites it and
`dolt_diff` shows one modified row per open bucket per run. That is
correct and cheap; it just means "rows changed" is never zero.

**Fetch.** Per device: one `curl` (plain, not latchkey — no
credential, and `device.iqair.com` was not behind Cloudflare's
challenge today), parse, one `bulk_upsert_in_tx`, then
`UPDATE airvisual_devices SET last_<tier>_ts_ms = MAX(...)`. No cursor
to resume from: every run fetches the same thing. The failure budget
and the per-device `warn!` come over from yolink unchanged. Unlike
yolink, a `reset_and_redownload` cannot recover anything older than the
windows, so it should say so in its summary rather than look like a
backfill.

**Render.** `yolink_render` nearly as is: `parse.rs` reads the two
tables through `open_reader`, `units.rs` gets a new `METRICS` table
(three PM quantities on one plot, CO₂, temperature, humidity, pressure,
AQI), `plot.rs` and `render.rs` are unchanged in kind. One page, one
`Sensor Timeseries` grid row plus one `Sensor Device` row each. The
tiers want either one trace per tier or the instant trace only with
the coarser tiers filling the gaps where instant is missing; the second
is what a person wants to see and is a few lines in `parse.rs`.

This is the point at which the time-series family should stop being
two copies. With a second provider the shared part is visible —
`<p>_devices` + `<p>_readings` keyed `device#ts#metric`, the
`Quantity`/`MetricSpec` tables, `plot.rs`, the one-page render and its
HEAD-vs-cursor skip — and a `datalib_etl_timeseries_render` crate
holding it, with each provider contributing only its metric table and
its parser, is the refactor to do *after* AirVisual lands, not before
(rule of two, and yolink's render is the golden that proves the
extraction changed nothing).

**Wiring.** The usual list, all of which yolink shows how to do:
`SourceType`, `Provider`, `dispatch.rs`, `ingestMethods.json` (via the
generator), `catalog.ts` (`wizard: false` first, as yolink), a stanza
in `all_sources.toml`, and `schema_inventory`.

**Tests.** The two captured responses are the fixtures: they cover
both generations, and station B's stale-since-July `historical` is
the dead-sensor case for free. Insta snapshots of the parser over each,
an upsert-idempotency test, and a `render_e2e` like yolink's. A live
test against a public station is possible (no key) but not needed for
correctness; it would only guard against IQAir changing the shape,
which the KB says it has done at least once.

**Effort.** Config crate ~150 lines, ingest ~400 (yolink's 739 minus
the windowing and signing), render mostly copy, wiring ~10 files. A
day or two, most of it the metric table and the fixtures.

## Open questions

1. **Which units?** "Monitoring stations" reads as AirVisual Outdoor,
   which is Route A only; a Pro adds Route C.
2. **Are they published as public stations?** If so the device API and
   the dashboard export are both free; if not, both may be behind a
   Dashboard subscription and the device-API URL for a private device
   needs a real one to test against.
3. **Is there a Dashboard subscription?** Decides whether Route B is
   worth building at all.
4. **A device id to probe.** One real id, from the dashboard's
   device-API link, turns the private-device questions above from
   guesses into a measurement in one `curl`. It is not a password.
5. **`airvisual` or `iqair` as the type?** The rule says product; the
   person says "IQ Air". One word to settle before the crate names are
   in git.
