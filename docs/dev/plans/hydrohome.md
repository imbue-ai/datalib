# HydroHome (Powerley) energy data: what's ingestable and how

**Status: proposal (2026-09-14). Nothing here is built.** The
architecture below was reverse-engineered from the shipping Android app
(`com.powerley.hydrohome`, versionName 1.1.1, versionCode 3301,
pulled off a real phone on 2026-09-14) — endpoints, hosts, auth mode,
MQTT topics and the OAuth server's advertised capabilities are all
quoted from the APK or from the live OAuth discovery document. What is
**not** yet verified is the exact request/response shape of any call and
the per-bridge MQTT topic prefix; both need one runtime capture. Where a
claim rests on a string in the binary rather than an observed exchange,
it says so.

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
| Per-minute / per-3s demand | AWS IoT Core MQTT, mutual-TLS | **yes** | live-only stream |
| Hourly history (~13 mo) | "Brainstem" REST API | yes | queryable batch, 1–2 day delay |
| Per-minute *history* | on-phone SQLite cache | no | superseded by tapping the stream |

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

### Hourly history — the "Brainstem" REST API

The app's stored usage history goes through a service it calls
**Brainstem**, on an AWS API Gateway
(`https://3xjvwrqe6e.execute-api.us-east-1.amazonaws.com`). Relevant
paths pulled from the binary: `/v3/brainstem`, `/metadata/intervaltypes`,
`/v3/cortex/clusterdata` and `/v3/cortex/digdeeper` (the usage-breakdown
/ disaggregation cards). The use-case names spell out the flow:
`GetUsageDataFromBrainstemAndSaveToDatabaseUseCase`,
`getUsageFromBrainstemAndSaveToDatabase`, `getIntervalTypes`,
`PurgeOldUsageDataUseCase`.

Granularity is **hourly**, not minute. The binary states
`smartMetersSendTheUsageDataOnceADay` and carries a
`DailyMinimumGranularityDbEntity` / `minimumGranularityType` concept — the
meter's stored history bottoms out at the hour, and it lands once a day
with the same 1–2 day validation delay BC Hydro's own MyHydro shows. This
is the same hourly data a MyHydro scrape would yield; fetching it via
Brainstem is just cleaner than scraping the billing site.

### Per-minute history — the phone's local cache (dead end)

The app persists the live stream into an on-device SQLite database
(`UsageDataPointDbEntity`, `UsageDataDbEntity`, `BrainstemDatabase…`,
rolled off by `purgeOldUsage`). That is the only place minute-resolution
*history* lives — the cloud relays the live feed but the evidence says it
does not archive it at minute resolution. We can't read that DB: the app
is not debuggable (`flags=0x0`, no `DEBUGGABLE`), and `adb backup` is
dead on the phone's Android version, so extraction needs root. It doesn't
matter — if we tap the MQTT stream ourselves we build the same history
from the source.

## Authentication

The OAuth server is a standard IdentityServer at
`https://powerboxauth.pwly.io`. Its live discovery document
(`/.well-known/openid-configuration`, fetched 2026-09-14) advertises:

- `authorization_endpoint`: `/connect/authorize`,
  `token_endpoint`: `/connect/token`
- grant types: `authorization_code`, `refresh_token`, `password`,
  `client_credentials`, `implicit`, `device_code`
- PKCE: `S256` and `plain`
- scopes: `openid`, `email`, `profile`, `readcustomer`, `offline_access`

Two consequences. First, `offline_access` + `refresh_token` means a
headless mirror can hold a long-lived refresh token and never prompt
again after first login. Second, `authorization_code` + PKCE is the path
the app almost certainly uses (BC Hydro sign-in is an SSO redirect); the
`password` grant is *advertised* and would be far simpler for a headless
client, but whether the backend actually honours it for these accounts is
unverified — do not assume it.

### Leaked secrets (a finding, not a dependency)

`assets/flutter_assets/.env` ships in the APK in plaintext and contains
`BCH_PROD_OAUTH_CLIENT_SECRET`, `BCH_PREPROD_OAUTH_CLIENT_SECRET`, a
Localizely SDK token, and `CERTIFICATE_PASSWORD`. This is a Powerley
packaging mistake, not BC Hydro's. The OAuth client secrets are the mild
case — mobile apps are public OAuth clients that can't keep a secret, and
PKCE (which this server supports) is the correct substitute, so a
well-built client would embed none of these. `CERTIFICATE_PASSWORD` is
the sloppy one: a single client-cert password shared across all ~75k
installs, where the right design provisions a unique cert per device. We
note it because it is the key that unlocks the IoT client cert; we should
not build anything that *depends* on a leaked shared secret staying valid.

## How this would land in datalib

Two providers, cleanly separated by transport and by how buildable they
are today.

### `hydrohome` — hourly history (buildable now, from anywhere)

An ordinary `origin` ingest, structurally almost identical to `yolink`:
authenticate once (OAuth/PKCE against `powerboxauth.pwly.io`, keep the
refresh token), then page Brainstem for interval data and mirror it into
a doltlite raw store keyed per reading (`meter#ts_ms#type`, the way
`yolink_readings` keys `device#ts_ms#metric` — see
[`providers/yolink/INGEST.md`](../../datalib/backend/etl/providers/yolink/INGEST.md)).
It inherits yolink's central warning verbatim: **sync cadence is data
retention** if the upstream window is bounded, and an empty window is
indistinguishable from a quiet one. The one new piece over yolink is the
OAuth token step; `latchkey`'s cookie capture doesn't fit a mobile OAuth
flow directly, so this needs its own auth handling.

### `hydrohome-live` — real-time demand (needs an always-on subscriber)

This is the first thing datalib would want that is **not** a batch step:
a resident process that stays subscribed to AWS IoT and appends each
demand sample as it arrives. The DAG has no concept of a long-lived step
today, so the honest shape is two parts:

1. A standalone collector (its own daemon, or a launchd/systemd unit the
   operator owns) that connects to `…iot.us-east-1.amazonaws.com` with
   the client cert, subscribes to the bridge's demand topic, and writes
   samples to a plain SQLite file.
2. A `hydrohome-live` ingest step that reads that file — the same
   "mirror a SQLite file someone else fills" pattern as the
   `sqlite_mirror` providers (`whatsapp`, `apple_photos`).

Whether the collector should eventually become a first-class datalib
daemon, or stay an external feeder, is a real design question and is
called out in "Open questions" below rather than answered here.

## What's blocking, and the one capture that unblocks it

Static analysis gave us hosts, paths, auth *mode*, and topic *names*. It
cannot give us the exact request/response bodies, the per-bridge topic
prefix (the bridge's `thingName`/clientId), or a usable client
certificate. All of those come from **one runtime capture**:

- **mitmproxy on the REST + OAuth traffic** (HTTPS, capturable): yields
  the OAuth exchange, the Brainstem request/response shape, and the
  `/v3/energybridge/GetCertificateForExisting` call — enough to build the
  hourly provider outright and to *mint* an IoT cert. Flutter ignores the
  system proxy and pins to a degree, so this may need `--set
  proxy_mode` tricks or an emulator with a system CA; details when we do
  it.
- **frida on the running app** (needs root / emulator): can dump the
  resolved MQTT topics, the client cert, and the demand-message JSON
  directly off the live screen — the surest way to nail the streaming
  side, at the cost of a bigger setup.

Start with the mitmproxy REST capture; it unblocks the whole hourly
provider and most of the live one.

## Open questions

- **Does Brainstem serve minute *history*, or only hourly?** Everything
  points to hourly-only, but this is inference from strings
  (`smartMetersSendTheUsageDataOnceADay`,
  `DailyMinimumGranularityDbEntity`), not an observed 404. One Brainstem
  capture settles it. If it does serve minutes, the live collector may be
  unnecessary for anything but sub-minute resolution.
- **Which OAuth grant actually works headless?** `password` would make a
  daemon trivial; `authorization_code`+PKCE means a one-time browser
  login then refresh tokens. Confirm against the token endpoint before
  designing the auth step.
- **Where does the live collector run, and is it a datalib daemon?** The
  feed is location-free, so "anywhere with the cert" — but datalib has no
  resident-process model. Decide before building whether to grow one or
  to keep the collector external and mirror its file.
- **Cert lifetime and renewal.** AWS IoT client certs can be rotated or
  revoked; a mirror that depends on one needs to know whether
  `GetCertificateForExisting` re-issues on demand and whether the leaked
  `CERTIFICATE_PASSWORD` is load-bearing for that call.
- **Powerlync vs Energy Bridge.** This plan is the Energy Bridge. The
  Powerlync exposes the same data locally as a HomeKit accessory (custom
  HAP service `DBDE3C5B-D7EA-434B-8684-356FAFAFD1A6`) and presumably the
  same AWS IoT path; if a second unit ever matters, confirm the topic
  shape is shared.
