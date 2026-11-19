# Baby Connect: how the download works

The `baby_connect` source copies one Baby Connect account: every entry
logged for each child (a feed, a sleep, a diaper, a dose of medicine, a
weight, a milestone, a diary entry), the note typed with it, and who
logged it. Each kind of object gets its own table of `(id, payload)`
rows, keyed by Baby Connect's id; the payload is the object as the web
app was sent it. The render half, `baby_connect_render`, turns each
child into one document (`../baby_connect_render/TRANSLATE.md`).

Baby Connect publishes no API. The download makes the two requests the
web page at `app.babyconnect.com` makes, as its own scripts make them
(`common.js`, `scstatus-4.js`, read November 2026).

## Signing in

The session is two cookies. `seacloud1` is set when a person signs in.
`srf` is set on every page, signed in or not, and is half of a
double-submit check: a list request carries a token from the home page
(`__srf_token__`), and the server answers `{"Code":401}` unless the
`srf` cookie sent with it is the one that token was made for. Neither
`JSESSIONID` nor anything else matters. So latchkey keeps both cookies,
through a registered service with a cookie-capture login:

```sh
latchkey services register babyconnect \
  --base-api-url="https://app.babyconnect.com/" \
  --login-url="https://app.babyconnect.com/login" \
  --login-flow=cookie-capture \
  --login-flow-params='{"cookieKeys": ["seacloud1", "srf"]}'
latchkey auth browser babyconnect
```

The wizard registers it the same way from the catalog entry's
`credentialRegister`. Capture waits until both cookies are set; `srf`
is set when the login page loads and `seacloud1` only once the sign-in
succeeds, so it never keeps a signed-out session. The login page does
not send a signed-in browser elsewhere, so signing in again once the
session has ended needs a person.

## What one run does

`src/ingest/mod.rs`:

1. **`GET /home2`**, the signed-in home page. Its inline script holds
   the token (`__srf_token__ = '…'`) and the account's children
   (`"myKids": [...]`), which are written to `kids`
   (`src/ingest/home.rs`). A page with neither is the login page a
   signed-out session is shown, and the run fails as a refused
   credential.
2. **`POST /CmdListW?cmd=StatusList`**, once per day, form-encoded:
   `Kid` (the one child's id, or `0` for every child when there are
   more, as the web page asks), `pdt` (the day as `YYMMDD`), `past=1`,
   `fmt=long`, `tzo=0;0` and the token. The answer's `list` holds the
   day's entries, plus a few from the days beside it; only entries
   whose `Pdt` is the day asked for are kept, so each entry is stored
   from its own day's answer. Its `users` are the people who logged
   them.

The days asked for run from `api.since`, else the eldest child's
birthday (`BDay`), to `api.until` or today, oldest first, less the days
`coverage` holds (scope `days`, spans of `YYYY-MM-DD`). A day's entries,
its people and its span land in one transaction. A day more than one
day old is recorded as read; the last two are not, since a parent may
log the night's feeds the next morning, so every run reads them again.
A day whose eve is held reaches back to it, so a run of days stays one
span (`days::span_for`).

Nothing else is kept as a position. A run stopped part-way leaves the
days it read recorded and asks for the rest next time.

**What a run costs.** One request per day the store does not hold: a
first run over four years is about 1,500. After that a daily sync is
three requests: the home page and the last two days.

## The store's shape

| table | key | written from | notes |
|---|---|---|---|
| `kids` | child id | the home page's `myKids` | name, birthday, parents. `url_t`, a signed picture address that changes every load, goes to the sidecar. |
| `users` | person id | each day's `users` | name and email of whoever logged; `url_t` to the sidecar |
| `entries` | entry id | each day's `list` | `Pdt` the day, `Utm` the clock time (`HHMM`), `Cat` the kind, `By` who logged it, `Txt` the app's sentence for it, `n` the typed note, `d` a duration in minutes. `readby`, which grows as people open the app, goes to the sidecar. |
| `coverage` | `(scope, lo)` | the walk | the days read for good |

Every row is stamped the first time it is seen
(`bulk_upsert_first_seen_in_tx`), so a day read again with nothing
changed commits nothing; an edited entry's payload is replaced.

Ids are JSON numbers past 2^53; serde_json keeps their digits, and the
table keys them as text.

## What it does not do

- **Times.** An entry's `Pdt` and `Utm` are a clock reading with no
  zone, and nothing Baby Connect sends names one: not the account, not
  the people, not the entry. `ms` is not the entry's moment, and `e` is
  when it was typed, often hours later. The render reads the reading in
  the configured `time_zone`.
- **Photos.** An entry's `photos` list is stored as sent; the pictures
  (`/GetCmd?cmd=PhotoGet&id=…`) are not downloaded.
- **Deletions.** A day's `deleted` list is not read, so an entry deleted
  in the app after its day was read stays in the store.
- **Comments.** An entry's `comments` are stored as sent and not
  rendered; none turned up in the account this was built against.
- **Years.** A day is two digits of year counted from 2000, as the web
  page counts them.

## How to inspect the result

Stock `sqlite3` cannot open a `.doltlite_db`; use the doltlite shell
(AGENTS.md § "Doltlite"):

```sh
datalib-doltlite -readonly <root>/<group>/ingest/entities.doltlite_db \
  "SELECT json_extract(payload,'$.Cat') AS cat, count(*) FROM entries GROUP BY cat ORDER BY 2 DESC"
datalib-doltlite -readonly <root>/<group>/ingest/entities.doltlite_db \
  "SELECT scope, lo, hi FROM coverage"
datalib-doltlite -readonly <root>/<group>/ingest/entities.doltlite_db \
  "SELECT id, json_extract(payload,'$.n') FROM entries WHERE json_extract(payload,'$.n') IS NOT NULL LIMIT 20"
```

## Tests

`tests/baby_connect_tests/` replays made-up TNG tapes (`tape.rs`) in the
shapes above. Beverly Crusher's account, with Wesley its one child,
covers a first run, a second run that asks only for the unsettled days,
an entry edited after its day, a `since`/`until` window and a
signed-out session. The Enterprise nursery's account, with Wesley and
Alexander, covers asking for every child at once.
