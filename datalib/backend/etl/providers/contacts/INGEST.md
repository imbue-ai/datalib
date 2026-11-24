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

**Not yet run against a live account.** The shapes below are the ones
Google's reference gives
([connections](https://developers.google.com/people/api/rest/v1/people.connections/list),
[person](https://developers.google.com/people/api/rest/v1/people),
[labels](https://developers.google.com/people/api/rest/v1/contactGroups/list)),
and the fixtures in `tests/contacts_tests/google_*.rs` are made up in
them.

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
| `addressbooks` | `google!people/<id>/connections` | the account's one list, named "Contacts", and its `sync_token` |
| `google_people` | `<address book>#people/<id>` | a contact, verbatim but for its `memberships`, which are a set and are stored sorted. `etag`s and each source's `updateTime` are in the sidecar's `volatile_payload` |
| `google_contact_groups` | `<address book>#contactGroups/<id>` | a label, or one of Google's own groups (`myContacts`, `starred`). Who is in it is each contact's `memberships` |
| `google_photos` | `<contact's key>#<url>` | the photo Google lists for a contact, and the `blake3` of its bytes in `blobs.sqlite` once fetched |

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
  once. A photo already held is not fetched again for it.
- **A token is only good for the fields it was asked for with**, so a
  change to `PERSON_FIELDS` needs a rung on the ladder that clears
  `addressbooks.sync_token`.
- **A page with nobody on it has no `connections`.** What says a reply
  is a page is its `nextPageToken` or `nextSyncToken`; one with neither
  fails the listing.
- **A contact listed with no `resourceName`** could be any stored
  contact, so a listing that meets one deletes nothing it did not name,
  leaves the token where it was and says so on `listing:contacts`.

### Photos

`photos[].url` ends in the size it would serve (`=s100`); the download
asks for `=s512`. A `default` photo is a letter Google drew, and is not
fetched. The link needs no sign-in and goes out plain, only to an https
`googleusercontent.com` address. The link names the photo: a new photo
is a new link, so a new row, and the old one goes. A photo that will
not come, or is not an image, is a problem on its row, opens the
contact's page, and is asked for again on the next run.

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
