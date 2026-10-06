# Fastmail: which credential reaches what

Three sources read from Fastmail, over two protocols and three hosts:

| source | protocol | host | latchkey service |
|---|---|---|---|
| `email` (`jmap` table) | JMAP | `api.fastmail.com` | `fastmail` |
| `calendar` (`fastmail` table) | CalDAV | `caldav.fastmail.com` | `fastmail-dav` |
| `contacts` (`fastmail` table) | CardDAV | `carddav.fastmail.com` | `fastmail-dav` |

Both latchkey services are built in. `fastmail` also covers the blob
host (`*-www.fastmailusercontent.com`); `fastmail-dav` covers both DAV
hosts, which is why calendar and contacts share it. Everything below
was measured against a live account with latchkey 3.15 and 3.16.

## Three kinds of credential

Fastmail issues three, and each protocol takes only some of them:

| credential | made at | read-only? | JMAP | CardDAV / CalDAV |
|---|---|---|---|---|
| browser login (`latchkey auth browser fastmail`) | an OAuth sign-in | no — Fastmail offers no read-only scope | yes | accepted as `Bearer`, but latchkey files it under `fastmail`, which does not cover the DAV hosts, so it is never sent there |
| API token | Settings → Privacy & Security → Integrations → API tokens | a **Read-only access** box, and a choice of data | yes | **refused**: 401 as `Bearer` |
| app password | … → App passwords | a **Read-only access** box for "Contacts (CardDAV)" and for "Calendars (CalDAV)"; none for the combined "DAV (CardDAV/CalDAV/WebDAV)" | not tried | yes, as Basic `address:password` |

So a read-only mirror takes an API token for mail and two app passwords
for the rest: one for contacts, one for calendars.

**Fastmail enforces read-only itself**; it is not a hint. A read-only
app password's `DELETE` of a made-up card or event gets 405 where the
read-write one gets 404, and a read-only API token's `ContactCard/set`
fails with `accountReadOnly` before any id is looked up. The access
choice is enforced too: the contacts password gets 401 from the CalDAV
host, and the calendar password from the CardDAV host. A token's JMAP
session says what it can do: `isReadOnly` on each account, and only the
chosen capabilities under `accountCapabilities`.

## Two app passwords, two account names

latchkey keeps one credential per account name per service, and
replaces it without asking when the name is reused (the general rules
are in [`latchkey.md`](latchkey.md#accounts-who-names-them)).
Contacts and calendar share `fastmail-dav`, so their two read-only
passwords need two names. The wizard suggests `<address> contacts` and
`<address> calendar` (`credentialPaste.accountSuffix` in
`datalib/ui/src/config/catalog.ts`); from a terminal:

```sh
latchkey --account "you@fastmail.com contacts" auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
latchkey --account "you@fastmail.com calendar" auth set fastmail-dav -u "you@fastmail.com:$(pbpaste)"
```

With both stored, latchkey refuses a request that names no account, so
each source names its own in `latchkey_settings.account`. The mail
login can keep the bare address as its name: it is on another service.

## Reading a failure

- **"the carddav server refused the credential latchkey sent"** (or
  caldav): every discovery URL answered 401 or 403, so latchkey sent a
  credential and Fastmail turned it away. In order of likelihood: an API
  token pasted where an app password belongs; an app password whose
  access is the other protocol; a revoked password. A lapsed
  subscription looks the same — on a lapsed trial Fastmail refuses app
  passwords for every legacy protocol while JMAP keeps working, so an
  IMAP login with the same password is the tell.
The next two arrive inside **"no carddav request reached the server"**
(or caldav), which says latchkey sent nothing, so neither the server URL
nor the password was tested:

- **"No credentials found for fastmail-dav (account '…')"**: latchkey
  holds nothing under that name, and nothing was sent. `latchkey auth
  list` shows the names it does hold.
- **"Multiple accounts are stored for service 'fastmail-dav'"**: the
  source names no account and latchkey holds more than one.
- **A calendar password marked "expired" in the wizard** (latchkey's
  `credentialStatus: invalid`) may be fine. latchkey checks every
  `fastmail-dav` credential with a `PROPFIND` on
  `carddav.fastmail.com/dav/principals/`, where a password with only
  calendar access gets 401 ([imbue-ai/latchkey#167](https://github.com/imbue-ai/latchkey/issues/167)).
  **Check connection** is the real answer.

## Checking a credential by hand

latchkey prints only a status here, never the secret:

```sh
latchkey --account "you@fastmail.com contacts" curl -sS -o /dev/null -w '%{http_code}\n' \
  -X PROPFIND -H 'Depth: 0' https://carddav.fastmail.com/dav/
```

207 means it reads. To see the source's own view, run its probe — the
same call the wizard's **Check connection** makes (add
`--list addressbooks` for what a picker's **Load** fetches):

```sh
echo '{"fastmail": {}, "latchkey_settings": {"account": "you@fastmail.com contacts"}}' > p.json
datalib-step probe contacts --params-file p.json
```

The DAV discovery quirks (where it starts, CDATA everywhere, the inbox
and outbox collections) are in the calendar provider's
[`INGEST.md`](../../datalib/backend/etl/providers/calendar/INGEST.md#fastmail-caldav--measured-against-a-live-account).
