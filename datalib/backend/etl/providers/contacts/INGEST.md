# contacts — what each upstream does

A contacts source reaches its cards one of three ways, each keeping
what its upstream sent:

- **CardDAV** (`fastmail`, `carddav`) and **`.vcf` files** (`vcf`) keep
  vCards, one row per card in `contacts`, keyed
  `{address book id}#{UID}`. The CardDAV loop is
  `datalib_etl_web::dav::sync`, which the calendar source shares; what
  Fastmail was measured to do is in the calendar source's
  [`INGEST.md`](../calendar/INGEST.md#fastmail-caldav--measured-against-a-live-account).
- **Google** (`google`) keeps the People API's JSON, below.

## Google Contacts

The reference:
[connections](https://developers.google.com/people/api/rest/v1/people.connections/list),
[person](https://developers.google.com/people/api/rest/v1/people),
[labels](https://developers.google.com/people/api/rest/v1/contactGroups/list).
What is marked *measured* was seen on one Google Workspace account of 95
contacts, which
`bazel run //datalib/backend/etl/providers/contacts:contacts_live` checks
again, with made-up contacts and labels added, edited, merged and deleted
on it. A contact given a new resource name (`previousResourceNames`) and
an expired token were not seen, so those are the reference's shapes and
have only run on the made-up fixtures in
`tests/contacts_tests/google_*.rs`.

### Sign-in

latchkey's built-in `google-people` service
(`latchkey auth browser google-people`), which asks for
`https://www.googleapis.com/auth/contacts`. That scope is read-write;
the download only sends `GET`s. It does not reach two lists, which are
not mirrored:

- **Other contacts**, the addresses Google saved because you wrote to
  them (`otherContacts.list`, scope `contacts.other.readonly`);
- **the organization's directory** (`people.listDirectoryPeople`, scope
  `directory.readonly`).

### The raw store

| table | key | holds |
|---|---|---|
| `accounts` | `google` | the signed-in account: `principal_href` is its resource name (`people/<id>`), `login` its address |
| `addressbooks` | `google!people/<id>/connections` | the account's one list, named "Contacts" |
| `sync_scope_state` | `<address book>:sync_token`, `<address book>:labels` | the sync token, which differs with every listing, and the choice of labels it was listed under |
| `google_people` | `<address book>#people/<id>` | a contact, verbatim but for its `memberships`, which are a set and are stored sorted. `etag`s, each source's `updateTime` and each photo's `url` are in the sidecar's `volatile_payload` |
| `google_contact_groups` | `<address book>#contactGroups/<id>` | a label, or one of Google's own groups (`myContacts`, `starred`). Who is in it is each contact's `memberships` |
| `google_photos` | `<contact's key>#<source>`, the source being `PROFILE/<id>` or `CONTACT/<id>` | the photo Google lists for a contact, and the `blake3` of its bytes in `blobs.sqlite` once fetched. The link they came from is the sidecar's `held_version` |

With `google.labels` set, a contact with none of those labels is not
stored, nor is a label not named; Google's own groups always are. The API
cannot be asked for one label's contacts, so every contact is still
listed, and the choice the sync token was listed under is kept beside it
(`sync_scope_state`, `<address book>:labels`): a delta does not name the
contacts a new choice takes in, so a run under another choice lists
everyone again and drops what it leaves out. A name that matches no label
is a `config:labels:` problem and deletes nothing, and a run where none
matches fails.

A login for a different account has a different address book, so the
first run with it drops the other account's contacts.

### What one run does

1. `GET /v1/people/me`: whose contacts these are.
2. `GET /v1/contactGroups`: every label, listed whole. A label whose
   content has not moved is not written again, and one the listing does
   not name is deleted.
3. `GET /v1/people/me/connections?requestSyncToken=true`, with the
   `syncToken` the last run ended on. With one, the reply is what
   changed since: an edited contact whole, a deleted one as
   `metadata.deleted`, and a contact Google gave a new resource name
   with its `previousResourceNames`, whose rows go. Without one, it is
   every contact, and what it does not name is deleted. Each page is one
   transaction, and the token moves in the last, so a listing cut off
   part way starts again from the token it had.
4. Each photo listed and not held, from `googleusercontent.com`.

So a run with nothing new is three requests, whatever the size of the
address book, and a changed photo is one more.

- **A sync token lasts seven days.** Google refuses an older one with
  the reason `EXPIRED_SYNC_TOKEN`; the run then lists every contact
  once. A photo held at the link it is listed at is not fetched again
  for it. A token Google never issued is refused as a plain
  `INVALID_ARGUMENT` with no reason (*measured*), which fails the
  listing.
- **A token is only good for the fields it was asked for with.** Google
  does not refuse one sent with other fields (*measured*), so nothing
  would say a new field is missing from the contacts that did not
  change: a change to `PERSON_FIELDS` needs a rung on the ladder that clears
  `sync_scope_state`'s `:sync_token` rows.
- **A page with nobody on it has no `connections`.** What says a reply
  is a page is its `nextPageToken` or `nextSyncToken`; one with neither
  fails the listing.
- **A contact listed with no `resourceName`** could be any stored
  contact, so a listing that meets one deletes nothing it did not name,
  leaves the token where it was and says so on `listing:contacts`.

- **Merging two contacts** in Google's page keeps one, with everything
  both said, and sends the other as `metadata.deleted` (*measured*).
  Deleting a label sends each contact that was in it as changed. Both
  arrive in the delta like any edit.
- **A delta can lag a write by some seconds** (*measured*), a contact's
  more than a label's. The run in between sees nothing new, and the next
  one's token still covers the write.

### Photos

`photos[].url` ends in the size it would serve (`=s100`); the download
asks for `=s512`. A `default` photo is a letter Google drew, and is not
fetched. The link needs no sign-in and goes out plain, only to an https
`googleusercontent.com` address. A photo that will not come, or is not an
image, is a problem on its row, opens the contact's page, and is asked
for again on the next run.

**A link is not a photo's name.** Most stay the same from one listing to
the next, but Google signs some afresh every time (*measured*: 2 of 33,
1,100 characters against 100, both `PROFILE` photos, the same bytes). So a
photo is keyed by where Google has it from, its link is volatile, and a
link that is not the one held is fetched again: the bytes say whether the
photo changed, and one that did not renders nothing again. Only a listing
that names the contact does this, so a run with nothing new still fetches
none.

### What a contact carries (*measured*)

- Every contact had a `CONTACT` source; most also had `PROFILE` and
  `DOMAIN_PROFILE` ones, Google's own account and the Workspace
  directory's entry for the same person. Both give the same names,
  addresses and numbers again, so the page shows one of each.
- Every contact had photos, but two in three had only letters Google
  drew.
- `canonicalForm` comes with some numbers: one written with its `+` and
  country code, and a US one with its area code (`(202) 555-0101`), but
  not a seven-digit number, a UK one written `020 …`, or eight ten-digit
  US-shaped ones already on the account.
- A `DOMAIN_PROFILE` location's `value` is the word `desk`; `buildingId`
  and `floor` say where, `$$NO_BUILDING$$` standing for no building.
- A label is a `USER_CONTACT_GROUP` with `metadata.updateTime`; Google's
  own eight groups are `SYSTEM_CONTACT_GROUP` and have no `metadata`.
  Every membership named a group the label listing gave, or was a
  `domainMembership`, which names none.
- `https://contacts.google.com/person/<id>` and `/label/<id>`, the ids of
  the resource names, open the contact's and the label's own pages.
- Two listings in a row agree but for the photo links: same order, same
  `etag`s and `updateTime`s. The sync token differs every time.

### Inspecting the result

Stock `sqlite3` cannot open the store; `datalib-doltlite` can (or
`.dump` it into plain SQLite first):

```sh
db=<root>/<source id>/ingest/entities.doltlite_db
datalib-doltlite -readonly $db "SELECT resource_name, display_name FROM google_people ORDER BY display_name;"
# Who is in each label.
datalib-doltlite -readonly $db "SELECT g.name, p.display_name FROM google_people p, json_each(json_extract(p.payload, '$.memberships')) m JOIN google_contact_groups g ON g.resource_name = json_extract(m.value, '$.contactGroupMembership.contactGroupResourceName') ORDER BY 1, 2;"
# Photos listed and not fetched, with why.
datalib-doltlite -readonly $db "SELECT ph.id, b.last_error FROM google_photos ph LEFT JOIN google_photos_bookkeeping b ON b.id = ph.id WHERE ph.blake3 IS NULL;"
```
