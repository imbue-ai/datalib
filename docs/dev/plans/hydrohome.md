# HydroHome (Powerley) energy data: what's ingestable and how

**Status: proposal (2026-09-14). Nothing here is built.** The
architecture below was reverse-engineered from the shipping Android app
(`com.powerley.hydrohome`, versionName 1.1.1, versionCode 3301,
pulled off a real phone on 2026-09-14) and then **confirmed by a live
traffic capture the same day** (arm64 emulator + Frida TLS-bypass +
mitmproxy, logged into a real BC Hydro account). Endpoints, hosts,
auth mode and MQTT topics come from the APK; the REST request/response
shapes and the usage granularities come from the capture and are marked
**[verified 2026-09-14]** where they do. What is still *not* observed:
the per-bridge MQTT topic prefix and the real-time IoT payloads (the
capture setup breaks the mutual-TLS MQTT connection, so "Live" was
unavailable in the emulator — see below), and the exact retention depth
of minute-resolution history.

**The one finding that rewrites this doc:** the cloud serves
**per-minute history** on demand (`granularity=2` → 1440 points/day),
reachable from anywhere with a bearer token. An earlier draft claimed
the cloud kept only hourly data and that minute resolution needed an
always-on MQTT subscriber. That was wrong. A `hydrohome` provider that
polls one REST endpoint gets minute data without any resident process —
see "The verified API" below.

## What HydroHome is

BC Hydro's HydroHome app is a white-label of **Powerley**'s home energy
platform (the package name is literally `com.powerley.hydrohome`; the app
ships assets for several other Powerley utilities beside `bch`). A
customer optionally buys a hub that reads their smart meter over Zigbee
Smart Energy Profile and relays usage to the app. Two hubs exist; this
plan is written against the one we have on hand, the **Energy Bridge**
($179), and notes where the **Powerlync** ($75) differs.

The app is **Flutter**: all business logic is AOT-compiled into
`lib/arm64-v8a/libapp.so`, so there is no Java to decompile. Everything
below came from string extraction over that binary plus the bundled
`assets/flutter_assets/.env`.

## Three datasets, three transports

The app surfaces three different things, and they arrive three different
ways. Only two are reachable without being on the home LAN — which is the
whole point, because the motivating user is on sabbatical abroad while the
meter is in Canada.

| Data | Transport | Off-LAN? | Nature |
|---|---|---|---|
| **Minute + hourly + daily history** | `bch-coreapi.pwly.io` REST (bearer) | **yes** | **queryable batch — this is the one to build** [verified 2026-09-14] |
| Real-time "right now" demand | AWS IoT Core MQTT, mutual-TLS | yes | live-only stream; only needed for sub-minute "now" |
| Per-minute *history* on the phone | on-phone SQLite cache | no | irrelevant — the REST API already serves minute history |

The important correction over the first draft: **the REST API serves
minute-resolution history**, so the real-time MQTT stream is not needed
to mirror per-minute data. The stream matters only if you want the
live "right now" number streamed continuously; for a datalib mirror,
polling the REST endpoint is enough and far simpler.

### Real-time demand — AWS IoT Core MQTT

This is the interesting one, and the reason "you need a box at the house"
is **false**. The live feed is cloud pub/sub:

- The Energy Bridge holds a permanent secure-MQTT connection to
  `a72x5ebt1guef-ats.iot.us-east-1.amazonaws.com` (an AWS IoT Core ATS
  endpoint) and publishes demand samples to it.
- The app connects to that *same* endpoint from anywhere and subscribes
  to the bridge's topic. The bridge→cloud and app→cloud legs are
  independent, so the subscriber's location is irrelevant. A phone in
  Switzerland sees a meter in Canada with no home-network hop.
- While the app is foregrounded it publishes down to the bridge
  (`metering/request_fast_poll`, `metering/polling_mode/set`) to raise
  the sample rate from 30s to 3s. BC Hydro's FAQ describes this from the
  outside as "3-second intervals while using the app, 30 otherwise,
  displayed as one-minute averages."

Metering topics seen in the binary (per-bridge prefix not yet resolved):
`event/metering/instantaneous_demand`, `metering/summation/minute`,
`meter/electric/instantaneous`, `meter/electric/summation`,
`metering/configure`, `metering/reset`, `metering/polling_mode/set`,
`metering/request_fast_poll`.

Authentication is **X.509 mutual TLS**, not a username/password. The
evidence: the Dart `x509` and `pointycastle` packages are linked, the
binary generates a CSR (`package:x509/src/request.dart`), the API exposes
`/v3/energybridge/GetCertificateForExisting` and a
`GetEnergyBridgePublicCertificateUseCase`, and the bundled `.env` carries
a `CERTIFICATE_PASSWORD` that unlocks the client key material. So the app
authenticates to AWS IoT with a client certificate it either mints (CSR →
API) or fetches for an already-registered bridge.

**The catch that survives all of this:** MQTT is live-only. It streams
"now" and does not replay a gap. Mirroring per-minute data therefore
needs a process that stays subscribed and appends each sample as it
arrives — but that process can run *anywhere* with the cert, not at the
meter.

### The verified API — usage history at every granularity [verified 2026-09-14]

The app's stored usage goes through a service it calls **Brainstem**,
and the live host is `https://bch-coreapi.pwly.io` (the
`execute-api.us-east-1.amazonaws.com` gateway seen in the binary is a
fallback/other-env base; the shipping BC Hydro build talks to
`bch-coreapi.pwly.io`). Every call carries an `Authorization: Bearer
<token>` header.

The usage endpoint is **`POST /v3/brainstem/usage/filter`** (and a BC
Hydro variant `POST /v3/brainstem/usage/bchfilter` — identical shape).
Request body:

```json
{ "customerId": <int>, "customerSiteId": <int>, "serviceNetworkId": <int>,
  "usageMode": 0, "startDate": "<iso>", "endDate": "<iso>",
  "interval": { "granularity": <int> },
  "groups": [ { "groupby": "<str>", "values": [<int>] } ],
  "thingUuids": [ "<uuid>" ], "includeCalculated": true }
```

Response: `sites[].things[]`, each thing carrying a `usage[]` array of
`{ timeStamp, totalUnit, cost, costContainers }` points plus a
`usageSummary { min, max, avg, sum }`.

**`interval.granularity` is the resolution dial.** Observed values:

| granularity | resolution | evidence |
|---|---|---|
| **2** | **1 minute** | a single day returned `usage` of length **1440** |
| 5 | 1 day | 379 points spaced 86400s apart |
| 6 | 1 week | 55 points spaced 604800s apart |

(1, 3, 4 = presumably 15-min / hour / month; 7, 8 = month / year. Not
all observed. `usageMode=0` = consumption.) So minute, hour and day all
come from one endpoint by changing one integer — exactly the dial a
provider needs. Many of a day's 1440 minute slots come back null (the
meter reports every few minutes and the cloud pads the grid), which is
why the app's Day view looks like it updates "every few minutes."

The identifiers the usage call needs are handed out by a few
bootstrap calls, all bearer-authenticated:

- `POST /v3/Login/12` → the session (the `12` is BC Hydro's service
  network id). Auth to it is bearer; the token itself originates from
  the Chrome SSO against `app.bchydro.com` (MyHydro credentials) — that
  leg was deliberately left un-intercepted, so the exact token mint is
  not captured, but every API call's `Authorization: Bearer` header
  shows the scheme.
- `POST /v3/brainstem/customer/find` → `customerId`, `serviceNetworkId`,
  per-site `things[]` with their `uuid`s (the meter `thingUuids`).
- `GET /v2/customersite/<customerId>/<siteId>` → the site, and the
  `EnergyBridge { SerialNumber, MacAddress, Uuid, … }` object — the
  bridge identity.
- `POST /v3/brainstem/metadata/{thingtypes,thingcategories,intervaltypes,rategroups}`
  → the enumerations (a `thingtype` carries a `granularity` and
  `fuelType`; `intervaltypes` carries the billing periods).
- `POST /v3/cortex/{digdeeper,mainusesofenergy,budget,advisorcards,carousel}`
  → the disaggregation / "dig deeper" breakdown, budget and advisor
  cards. Not needed for a raw usage mirror, but this is where the
  per-appliance estimates live.

The remaining unknown is **retention depth**: minute data for recent
days (yesterday confirmed) is served, but how far back `granularity=2`
goes before the cloud only has hourly is not measured. Determine it by
requesting `granularity=2` for progressively older `startDate`s and
watching where the minute grid stops coming back.

### The older, binary-only reading (kept for contrast)

Before the capture, the binary alone suggested granularity was hourly:
`smartMetersSendTheUsageDataOnceADay`, a
`DailyMinimumGranularityDbEntity` / `minimumGranularityType` concept.
Those describe the *meter's* delivery cadence and its coarsest billing
granularity, not the API's finest — the capture shows the cloud holds
and serves the minute grid regardless.

### Per-minute history — the phone's local cache (moot)

The app also persists usage into an on-device SQLite database
(`UsageDataPointDbEntity`, `UsageDataDbEntity`, `BrainstemDatabase…`,
rolled off by `purgeOldUsage`). We can't read it (the app is not
debuggable — `flags=0x0` — and `adb backup` is dead on modern Android,
so it needs root), but it no longer matters: the REST API serves the
same minute grid, so there is nothing in that cache the API won't give
us directly.

## Authentication [verified 2026-09-14]

**The client does not do a standard client-side OAuth flow.** The
capture's entire auth surface was two things:

- `GET app.bchydro.com/sso/ui/login` (+ `login.js`) — **BC Hydro's own
  SSO login page**, where the user enters MyHydro credentials.
- `POST bch-coreapi.pwly.io/v3/login/12` — the call that mints the
  Powerley **bearer token** (the `12` is BC Hydro's service network id).

Across all captured flows there was **no `powerboxauth.pwly.io`, no
`/connect/authorize`, no `/connect/token`, no `/oauth`** — so there is no
`client_id` / `redirect_uri` / PKCE dance in the client path at all. The
OIDC server at `powerboxauth.pwly.io` (whose discovery doc advertises
`authorization_code`+PKCE, `refresh_token`, `offline_access` and even a
`password` grant) is Powerley's server-side / other-utility auth, **not**
what the BC Hydro app uses. Earlier drafts aimed the "PKCE vs.
client-secret" question at `/connect/token`; that was the wrong endpoint.

The real flow is: browser SSO login at `app.bchydro.com/sso/ui/login` →
BC Hydro hands the app a token/assertion → the app exchanges it at
`POST /v3/login/12` for the Powerley bearer → every API call carries
`Authorization: Bearer`.

**Consequence for getting a token: no MITM.** A normal browser login to
BC Hydro's SSO yields a session that mints the bearer — ordinary HTTPS,
real certificates, nothing intercepted. The MITM/Frida/emulator rig was
a one-time *reverse-engineering* tool; it is not part of the running
system and is not needed to authenticate.

### Using latchkey (imbue-ai/latchkey) — the intended auth layer

Auth belongs in `latchkey` (Imbue's own credential tool), exactly as the
`claude` and `chatgpt` providers do it, so the datalib provider embeds
**no auth code and no secrets** — it just calls `latchkey_curl` with its
service name. Two rungs:

1. **Works today, no latchkey change.** latchkey already supports custom
   services: `latchkey services register bchydro
   --base-api-url=https://bch-coreapi.pwly.io` then `latchkey auth set
   bchydro <bearer>`. Obtain the bearer from a browser SSO login (or,
   for a first test, from a capture). The provider is then structurally
   identical to `chatgpt/src/ingest/api.rs`. Downside: a static bearer
   expires, so re-run `auth set` — the same manual re-auth `claude` /
   `chatgpt` already have here.
2. **The seamless version** — add a `hydrohome` browser-login service to
   latchkey (your repo) that drives `app.bchydro.com/sso/ui/login`,
   captures the token, and refreshes it. This is latchkey's
   browser-capture pattern (cf. `latchkey auth browser chatgpt`), just
   with BC Hydro's SSO. It is a latchkey code change, not a datalib one.

### The one credential-bearing call still unexamined

`POST /v3/login/12`'s request/response bodies were deliberately **not**
dumped (they carry the SSO assertion and the minted token). That call is
where the publishability gate now lives: **does the login exchange
require the leaked `BCH_PROD_OAUTH_CLIENT_SECRET`, or only the user's SSO
assertion?** If only the assertion, a working client — and a `hydrohome`
latchkey service — embeds no shared secret and the provider is cleanly
publishable. If it needs the baked-in secret, that is the thing to design
around (or a reason to keep it private). Settling it means inspecting
that one call's shape, a deliberate separate step.

### Leaked secrets (a finding, not a dependency)

`assets/flutter_assets/.env` ships in the APK in plaintext and contains
`BCH_PROD_OAUTH_CLIENT_SECRET`, `BCH_PREPROD_OAUTH_CLIENT_SECRET`, a
Localizely SDK token, and `CERTIFICATE_PASSWORD`. This is a Powerley
packaging mistake, not BC Hydro's. The client secrets are the milder
case — a mobile app is a public client that fundamentally can't keep a
shared secret — but they are shipped to all ~75k installs regardless.
`CERTIFICATE_PASSWORD` is the sloppy one: a single client-cert password
shared across every install, where the right design provisions a unique
cert per device (it unlocks the IoT client cert, relevant only if the
live side is ever built). Whichever way the `/v3/login/12` question above
resolves, **do not build anything that depends on a leaked shared secret
staying valid** — if the login needs it, that is an argument for keeping
the provider private, not for embedding the secret.

## How this would land in datalib

### `hydrohome` — usage history, all resolutions (buildable now, from anywhere)

An ordinary `origin` ingest, structurally almost identical to `yolink`.
Authenticate once (bearer token; obtaining it is the one open auth
question below), run the bootstrap calls to learn `customerId` /
`customerSiteId` / `serviceNetworkId` / `thingUuids`, then walk
`POST /v3/brainstem/usage/filter` per day at `granularity=2` (minute)
and mirror the `{timeStamp, totalUnit, cost}` points into a doltlite
raw store keyed per reading (`meter#ts_ms#granularity`, the way
`yolink_readings` keys `device#ts_ms#metric` — see
[`providers/yolink/INGEST.md`](../../datalib/backend/etl/providers/yolink/INGEST.md)).
Because the same endpoint serves day and week granularities too, one
provider covers the whole Usage screen.

It inherits yolink's central warning: **sync cadence is data retention**
to the extent minute history ages out of the cloud (retention depth is
the open question below), and a null/empty slot is indistinguishable
from a genuinely quiet minute. The one new piece over yolink is the
bearer-token step; `latchkey`'s cookie capture doesn't fit this flow, so
it needs its own auth handling.

**No always-on collector is needed.** The earlier draft proposed a
resident MQTT subscriber for minute data; the REST endpoint makes that
unnecessary. A real-time `hydrohome-live` tapping AWS IoT is still
*possible* (for a continuously-updating "now" number) but is a
nice-to-have, not the path to per-minute history, and it would be the
first non-batch thing in the DAG — defer it.

## What's verified, and what still needs a look

The 2026-09-14 capture (arm64 emulator + Frida `ssl_verify_peer_cert`
patch + mitmproxy, real account) verified the host, the bearer auth,
the usage endpoint's request/response shape, and the granularity dial
(minute/day/week observed directly). Reproduce it with the toolchain in
the session notes: merge the app's split APKs to a universal APK, run it
under `disable-flutter-tls.js`, route the emulator through mitmproxy
with `bchydro.com` and the Google/reCAPTCHA hosts on **passthrough** (so
the Chrome SSO login page — which pins — still works), and read
`usage/filter` flows. Extract *shape only* (keys + value types), never
values: the responses carry personal usage and the requests carry
account ids.

## Open questions

- **Retention depth of minute history.** Yesterday's `granularity=2`
  returned a full 1440-slot day. How many days back before the cloud
  only has hourly is unmeasured — request `granularity=2` for older
  `startDate`s and find the cliff. This sets the minimum sync cadence.
- **Does `POST /v3/login/12` require the leaked client secret?** This is
  the publishability gate (see the Authentication section). The token is
  minted by BC Hydro SSO (`app.bchydro.com/sso/ui/login`) → an exchange
  at `/v3/login/12`; there is no client-side `/connect/token`. What that
  exchange takes as input — the SSO assertion alone, or the assertion
  plus `BCH_PROD_OAUTH_CLIENT_SECRET` — decides whether a working client
  embeds a shared secret. Settle it by inspecting that one call's shape
  (a credential-bearing call, so a deliberate separate step) before
  committing to "publishable."
- **The real-time MQTT side is still unobserved.** "Live" was
  unavailable in the emulator because the mutual-TLS MQTT connection to
  `a72x5ebt1guef-ats.iot.us-east-1.amazonaws.com:8883` can't be proxied
  by mitmproxy — it needs the client cert and a Frida hook on the Dart
  MQTT client to dump topics/payloads. Only worth doing if we decide we
  want the continuous live feed; per-minute history does not need it.
- **Cert lifetime and renewal** (only relevant if we build the live
  side). Whether `/v3/energybridge/GetCertificateForExisting` re-issues
  on demand and whether the leaked `CERTIFICATE_PASSWORD` is load-bearing
  for that call.
- **Powerlync vs Energy Bridge.** This plan is the Energy Bridge. The
  Powerlync exposes the same data locally as a HomeKit accessory (custom
  HAP service `DBDE3C5B-D7EA-434B-8684-356FAFAFD1A6`) and presumably the
  same AWS IoT path; if a second unit ever matters, confirm the topic
  shape is shared.
