# Attachments by address, not by copy

*Mirror of the "Attachments" tab of the [datalib links](https://claude.ai/code/artifact/dd31c9e3-8ef9-4185-a85f-0c29a1318117) Claude Doc, which is the source of truth while this plan is under review. Edit the doc and re-mirror this file; don't edit it here. Proposal (2026-10-20); step 1 landed in PR #1305.*

When datalib renders a page, it writes every attachment the page shows into a `blobs/` folder beside it, though the source's store already holds the same bytes. So every attachment is on disk twice, and most of the copies belong to old mail and chats nobody opens. The proposal: a page points at the bytes where they already are, at a `/data/…` address (first tab), and the server hands them out when someone opens the page.

**Status:** step 1 below (chat sources, Notion and Google Drive Docs as `blob` addresses, with the contract crate and the `/data/` route) landed in PR #1305. Email and contacts are not started.

## What happens today

- **Three places write the copies.** The shared chat renderer does it for email, Slack, Signal, WhatsApp, Claude, ChatGPT, Beeper, SMS, Google Takeout, Facebook and LinkedIn; Notion's renderer for its pages; the shared contact renderer for contact photos.
- **Email is a special case.** Its store holds only whole emails (`.eml` files), with each attachment inside encoded as text. Render cuts each attachment out and writes it decoded.
- **In the app, only images reach the server.** The UI points an image at the `/asset/<page uuid>/blobs/…` route, which reads the file beside the page. A download link or an audio or video player is left as `blobs/…`, which resolves against the app's own address. This is from reading the code; not yet tried in the app.

## Where the bytes already are

| Source | Bytes kept in | Address |
| --- | --- | --- |
| Chat sources, Notion, LinkedIn photos | the source's blob store, `blobs.sqlite` | `/data/<group>/ingest/blob/<hash>` |
| Email | inside the stored email, in the blob store | `/data/<group>/ingest/blob/<email hash>/part/<part hash>` |
| Contacts (CardDAV, `.vcf` files) | inside the stored contact card; the download will also put each photo in the blob store | `/data/<group>/ingest/blob/<hash>` |

An email attachment is cut out of its email each time it is asked for, so showing one image from a 25 MB message reads all 25 MB. Opening old mail is rare enough that this beats keeping a second copy of every attachment.

A contact photo is kept once more, in the blob store, so the server never has to read a contact card out of the source's versioned store. Photos are a few tens of KB each. A data root that already holds contacts gets a one-time pass that copies the photos of the cards it has.

## Serving them

- **`datalib-http` answers `/data/…` itself**, reading the step's blob store through the small crate that defines it (first tab, §3). It knows that file's shape and how to take an email apart, and nothing about any source.
- **The path is checked, never used as a file path.** The group must be one of the config's groups, so the path cannot reach `system/` or anything else that is not a group's. The function only has to be a plain directory name (not empty, not starting with `.`), on purpose: a render-only source's pages link the blob store in an `ingest` tree that no step of this config writes. The hash must be 64 lowercase hex digits. Anything else is "not found".
- **The blob store needs no special care to read**: it is a plain SQLite file that only ever holds finished writes, so opening it read-only while a sync writes to it is safe.
- **What an answer carries:** `nosniff` on every answer; the sandbox policy (`DATA_DOCUMENT_CSP`) on a document type that could run script, such as HTML or SVG; script itself sent as `application/octet-stream`, so no page can run it; and `Cache-Control: private, max-age=31536000, immutable`, since an address's bytes never change. Email gains most from the caching, since every fetch re-reads the message.
- **The choice stays easy to undo.** If serving should move to the search applet (first tab, §7), it moves as one route; no page or address changes.

## What render does instead

- It writes an address in place of each file. To do that it needs only an attachment's hash, type and name, so it stops loading attachment bytes at all. Email still loads each email, to render its text.
- An attachment whose bytes the store has lost is still reported on its message, as now.
- Every chat source and Notion renders once more in full after the upgrade, and render deletes the `blobs/` folder beside a page when it next writes that page.

## Before, after, and what a person notices

| Change | Before | After | What a person notices |
| --- | --- | --- | --- |
| Disk | each attachment kept twice | once | Less disk used; how much is to be measured on a real data root. |
| Sync time | render reads and writes every attachment's bytes | render reads hashes only | Faster renders for sources heavy with attachments; to be measured. |
| In the app | images shown; download links and players don't open | all of them go through `/data/…` | Links and players in chat pages open. |
| Outside the app | a page's `.md` file sits beside its attachments and opens in any viewer | the `.md` has `/data/…` paths | Viewing attachments needs a running datalib, until an export exists. |
| An agent reading the mirror | opens attachment files on disk | fetches `/data/…` with the API token | The agent guide says how. |
| The first sync after upgrading | — | every chat source and Notion re-renders | One slower sync. |

## Order of work

1. **Chat sources and Notion**, as `blob` addresses, with the contract crate and `datalib-http`'s `/data/` route. Email keeps writing its files until step 2.
2. **Email**, as `part` addresses; the code that writes copies goes.
3. **Contacts**: the download puts each photo in its blob store and pages point at it; LinkedIn's photos become `blob` addresses too. The code that writes photo files goes.

The old `/asset` route stays until nothing writes `blobs/`.

## Open questions

- [ ] **How much disk do the copies take on a real data root?** Worth measuring before and after step 1.
- [ ] **Delete old `blobs/` folders in one sweep at upgrade, or as each page is rewritten?**
- [ ] **Is an export for reading pages outside the app needed now, or later?**
