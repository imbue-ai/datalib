# The contacts app

Sources know a person only by **handles**: one identifier each, such as
an email address, a phone number or a Slack id. So one person arrives as
several, and nothing in a mirror says they are the same person. The
contacts app is where that gets said. A **contact** is a person's own
record, and a **link** says a handle belongs to it.

What a link gets you:

- **Every chip** shows the contact's name and photo for any of its
  handles, in every document and grid, at once.
- **Search** follows it: `with:contact:<id>` finds everything from or
  about the person under every handle linked there
  ([Searching your mirror](searching.md)).
- **Nothing re-renders.** Links sit over the view: no document changes,
  and neither index runs again, so a link costs nothing to make or undo.

The app also holds **labels** on people and senders (`real`: in when
the person or sender is not spam, out when it is) and every link a person
turned down. It is the one store under a data root that nothing can
rebuild, so its history keeps every change.

This page is for an agent asked to do that work. It is the steps; the
tables and the rules behind them are in [the reference](../dev/contacts.md).

## Reaching it

It is the applet `datalib_contacts`, served at
`/applet/datalib_contacts/` on `datalib-http`, with the API token. **It
answers only while the app is up.**

```sh
TOKEN=$(cat <data_root>/system/api-token)
curl -H "Authorization: Bearer $TOKEN" "<origin>/applet/datalib_contacts/"
```

An answer of 502 naming `no applet "datalib_contacts"` means this
library's config has no entry for it: a root set up before the app
existed. Turning it on is the person's call, in **Add data source** →
Contacts app, or with the entry [the applets page](applets.md#what-ships)
gives.

That answer lists every route, what each does and its `kind`; so does
`datalib-applet datalib_contacts --help`, with no server. `kind` is the
part to read before you write:

| `kind` | means | for an agent |
|---|---|---|
| `read` | changes nothing | yours to call |
| `propose` | recorded under your name, and waits for the person | yours to call: this is `POST /proposals` |
| `person` | recorded as the person's own decision, final, and never reviewed | **only when they asked for that very change** |

The `person` routes are what the app's buttons call: `POST /link`,
`POST /contacts`, `POST /label`, `POST /review/confirm` and the rest. The
store cannot tell who is on the other end of the token, so a link you
make with `POST /link` reads as the person's own, carries no reason, and
never appears for them to review. Confirming your own proposals erases
the same difference. When the person says "link this address to Riker",
calling it is what they asked for. When you are working through a
mailbox, it is not.

If the answer is a 502 saying `no applet "datalib_contacts"`, the config
has no entry for it. A root made by `datalib-dag init` or the app has
one; an older one may not. Add this and the app starts it:

```toml fragment
[[applets]]
group = "unified_document_index"
id = "datalib_contacts"
command = "datalib-applet datalib_contacts"
```

## Spelling a handle

A handle is `<kind>:<value>`, and these routes want it whole. Search
takes a bare `riker@enterprise.org`; `POST /proposals` skips it and says
why.

| handle | is |
|---|---|
| `email:riker@enterprise.org` | an address, lowercased |
| `tel:+12025550101` | a number with its country code, `+` and digits only |
| `slack:T01/U02` | a Slack user, by workspace and user id |
| `signal_aci:<uuid>` | a Signal account known by no number |
| `github:<login>`, `linkedin:<slug>` | those accounts |

The other kinds, and who writes which, are in [the
reference](../dev/contacts.md#handles). A number written without its
country code gets no handle, so it cannot be linked. Copy handles from the
query below rather than building them.

## 1. See what is known

The grid index holds every handle any source mentioned, and what that
source called the person. This lists them with the most written first:

```sh
datalib-doltlite -readonly <data_root>/unified_document_index/grid_index/db.sqlite "
  SELECT h.handle,
         group_concat(DISTINCT c.name) AS names,
         group_concat(DISTINCT c.source_id) AS sources,
         sum(c.seen_items) AS items
    FROM source_contact_handles h
    JOIN source_contacts c USING (markdown_uuid, contact_key)
   WHERE h.handle NOT LIKE 'me:%' AND h.handle NOT LIKE 'ai_model:%'
   GROUP BY h.handle
   ORDER BY items DESC
   LIMIT 200;"
```

`me:self` is the person themself and `ai_model:` a model's answers, so
neither is a contact to make. A sender's whole record is `contact_json`
on `source_contacts`, and the search shows what they wrote:

```sh
datalib-applet search <data_root> 'with:email:riker@enterprise.org' --limit 20
```

The same answer for a few handles at once, through the app, is `POST
/applet/unified_document_index/people` with `{"handles": […]}`.

## 2. See what is already decided

Read these before you propose, here on the contacts app:

```sh
curl -H "Authorization: Bearer $TOKEN" "<origin>/applet/datalib_contacts/contacts"
curl -H "Authorization: Bearer $TOKEN" "<origin>/applet/datalib_contacts/review/items"
curl -H "Authorization: Bearer $TOKEN" "<origin>/applet/datalib_contacts/not_linked"
```

- `contacts`: every contact, as a table (`columns`, `rows`).
- `review/items`: every contact or handle with anything proposed or
  labelled, and who said so. `reviewed_at_utc` is when a person looked,
  and `decided_how` of `manual` is their own answer.
- `not_linked`: every link a person turned down, with the `reason` that
  proposed it. Read these for what they tell you about this person's
  idea of a duplicate.

`POST /resolve` with `{"handles": […]}` answers which of those handles
have a contact already.

## 3. Propose

Send one batch, and name yourself and the batch:

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  "<origin>/applet/datalib_contacts/proposals" -d '{
    "agent": "claude",
    "run": "2026-11-mailbox",
    "contacts": [{
      "name": "Will Riker",
      "handles": ["email:riker@enterprise.org", "tel:+12025550101"],
      "reason": "the same number is in the signature of his mail",
      "confidence": 0.8,
      "labels": [{"label": "real", "reason": "they wrote back", "confidence": 0.9}]
    }],
    "handles": [{
      "handle": "email:offers@ferengi.test",
      "labels": [{"label": "real", "applies": false, "reason": "one-way offers", "confidence": 0.7}]
    }]
  }'
```

- `contacts` groups handles: into a new contact called `name`, or into
  an existing one with `contact_id` in place of `name`.
- `handles` labels a sender with no contact to make.
- `confidence` is 0 to 1, and the person reviews your least sure first.
  So give each one you can: a close call marked as one costs them less
  than a wrong one marked as sure.
- A label is any word. `applies` is `true` (the default) or `false`.
  `real` is the one to work on; the others stand alone, and nothing reads
  them but the person.

The answer says what landed and what did not:

```json
{"contacts_created": 1, "linked": 2, "labelled": 2, "skipped": []}
```

**Read `skipped`: the call still answers 200.** Each entry has a `what`
and a `why`: a handle someone else holds, a pair the person turned down,
a label they answered the other way, something that is not a handle. A
person's word stands, so don't send it again under another name. You
may send a batch again over your own rows nobody has reviewed yet.

## 4. Hand it to the person

Nothing you sent is final. Tell them it is waiting: on the Manage
screen, **Browse** on the contacts row opens the contacts card, and its
*Review* switch shows every row with something proposed or labelled, the
least sure first. Until they look, your links already draw on chips, so
say so.

They can confirm, reject, or answer a label themselves. Rejecting takes
your links into `not_linked` and turns your labels around, and a contact
of yours left holding nothing goes with them. All of it comes back to you
in `review/items` and `not_linked`.

## Saying who the person is

`from:me` knows the account each source marks as its owner's. If it
misses some of their handles, propose those handles as a contact and tell
them to tick *This is me* on its card. `POST /me` is that tick, and is
theirs. `from:me` then reaches every handle on that contact, as
[Searching your mirror](searching.md#me) says.

## What it does not do yet

Labels are not in search: there is no `label:real` to filter by, so a
label shows only on the contacts card. There is no route to add a member
to a group contact. The reference's [Not built](../dev/contacts.md#not-built)
has the rest.
