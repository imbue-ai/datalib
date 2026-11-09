# Showing a rendered document in your app

Every source's render step writes its records as markdown documents: an
email thread, a Slack channel's month, a calendar's week. The markdown
names people and times in two fixed shapes, so that any markdown viewer
shows something sensible and an app that knows the shapes can do more:
draw a person as who they are, and show every time in the zone its reader
wants. This page is that contract, for an app that shows documents
itself.

Datalib's own document view is built on exactly this. Nothing here is
private to it.

## Getting a document

The `unified_document_index` applet serves the documents, and
[the applets](applets.md) says how to call one:

- `GET /applet/unified_document_index/search?q=…` finds them. Each row
  names its document by `markdown_uuid`.
- `GET /applet/unified_document_index/chat/{markdown_uuid}` answers one,
  as JSON. `body` is the markdown, without its front matter; beside it
  are what the index knows of the document (`name`, `created_at`,
  `source_url`, `problems`, its links to other documents).

The markdown mixes markdown with a little HTML. It quotes what strangers
wrote, so **show it through an HTML sanitizer**, as you would any mail.
Keep at least the `<time>` and `<div>` elements and the attributes this
page names.

## The parts of a document

Each message, comment or record in a document is wrapped in a `div`
whose `id` and `data-section-uuid` name it:

```html
<div id="m-0190c5e3-…" data-section-uuid="0190c5e3-…" class="msg …">
```

That uuid is the message's own row in the search index, so a search
result can scroll to it, and `#m-<uuid>` links to it.

A message starts with a header line: the author, as a person link, then
the time:

```markdown
## [Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>") <time class="msg-ts" datetime="2369-04-15T10:05:00+02:00" title="2369-04-15 10:05:00 +02:00">Tue Apr 15th, 2369 at 10:05</time>
```

It is a real `##` heading because the search's chunker cuts at
headings. Datalib's viewer styles it down to one line; yours may too.

## A person

**A person is a markdown link whose href is who they are:**

```markdown
[Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>")
```

- **The text** is what the source showed for them: a display name, a
  Slack name, a phone number. Show it until you know better.
- **The href** is one of their handles written as a URI. It is the
  person's identity.
- **The title** says who it is in words, for any viewer that shows a
  link's title on hover: `Name <address>`, or `Name (identifier)` for a
  handle that is not an address.

The hrefs, using a standard scheme wherever one exists so that a link
pasted elsewhere still works:

| the handle | its URI |
|---|---|
| an email address | `mailto:riker@enterprise.org` |
| a phone number | `tel:+12025550101` |
| a Slack user | `slack://user?team=T01&id=U02`, Slack's own deep link |
| a LinkedIn member | `https://www.linkedin.com/in/<slug>`, their profile |
| any other kind | `datalib:handle/<kind>/<value>`, the value percent-encoded but for its `/`s: `datalib:handle/github/data-soong`, `datalib:handle/facebook/name/Will%20Riker` |

A handle is the URI's own spelling of it: `mailto:riker@enterprise.org`
is the handle `email:riker@enterprise.org`, and `slack://user?team=T01&id=U02`
is `slack:T01/U02`. The [people reference](../dev/contacts.md) lists every
kind.

### Trust the href, never the text

A person link can sit anywhere in a body, including in an email a
stranger wrote. That is safe only because **a viewer shows who the href
names, never who the text claims**. A body that writes
`[Picard](mailto:phisher@romulan.net)` is a link to the phisher. Draw it
under the name your sources know that address by, and say that the
document showed it as "Picard".

Only an explicit link is a person. A bare address that a markdown
viewer turns into a link by itself (`linkify`) is just an address in the
text, such as a signature's.

### Who a handle is

`POST /applet/unified_document_index/people` with
`{"handles": ["email:riker@enterprise.org", "slack:T01/U02"]}` answers
`{"people": {<handle>: [record, …]}}`: for each handle, every source's
record of that person, best first: a source about people (an address
book) ahead of a chat, then the chat where they wrote the most. A record
has their names, their other handles, a photo where a source had one,
and the source it came from. A handle no source mentions is absent from
the answer. Ask for every handle on the page in one request.

Where the library keeps a contacts app, a person may also have linked
handles into one contact of their own; the
[contacts app](contacts_app.md) answers that.

## A time

**Every time a document shows is a `<time>` element**, and its
`datetime` attribute is the whole value:

```html
<time datetime="2026-01-05T09:00:00+01:00" title="2026-01-05 09:00:00 +01:00">Mon Jan 5th, 2026 at 09:00</time>
<time datetime="2369-03-18">Tue Mar 18th, 2369</time>
<span class="time-range"><time datetime="2026-09-18T19:00:00-07:00" …>…</time> – <time datetime="2026-09-18T22:00:00-07:00" …>…</time></span>
```

- An **instant** is RFC 3339 with an explicit offset, to the second.
  `+00:00` is a clock that really read UTC; `Z` means the clock is
  unknown (below).
- A **day** is `YYYY-MM-DD`: the same day wherever it is read, so never
  convert it to an instant.
- A **span**, such as a calendar event's, is two of them in a
  `<span class="time-range">`.
- The **text** is a short form for a viewer that knows nothing of this;
  the **title** is the instant in full. Neither is meant to be parsed.

**Format the `datetime` yourself** in whatever zone and style your
reader wants. The text was written once, at render time, in the stamp's
own offset; redrawing every `<time>` from its `datetime` is how datalib's
viewer follows its status-bar clock without rendering anything again.

**The offset is the clock the time was written on**, so "as written"
shows an email at its sender's 09:00. A message's time takes the
nearest thing to its sender's clock that the source has:

| source | the clock |
|---|---|
| email | the `Date:` header's offset (for Fastmail, `sentAt`'s) |
| Google Chat, from a Takeout | the zone the export names (`CET`, `PST`) |
| Slack | the zone the author's profile is set to (`tz`), on the message's date, daylight saving included. It is their profile as of the last time Slack listed them, so a message from before a move reads on the new zone's clock |
| a calendar event | the event's own zone |

**`Z` says the clock is unknown.** Since RFC 9557, which updated RFC
3339, `Z` means "the instant in UTC is known, the local offset is not",
and `+00:00` means the time really was on UTC's clock. (RFC 3339 spelled
the first `-00:00`; read that the same way.) A message's time is written
with `Z` when its source records only an instant: a Slack author with no
profile zone, a phone backup's milliseconds (SMS, iMessage, Signal), a
service stamping its own UTC (a Drive or Todoist comment). Show such a
time on your reader's own clock rather than as UTC; datalib's viewer
does that under "as written". Any parser reads it as the same instant.

Other stamps in a page (an event's time, a page's "Last changed") are
written with an explicit offset, so a service's `Z` there still comes
out as `+00:00`.

A message with no time at all reads `<span class="msg-ts">(no timestamp)</span>`
rather than a made-up one.

## Attachments and images

An attachment, an inline image or a photo the mirror holds is linked at
an address on datalib's own server:

```markdown
[briefing.pdf](/data/gmail/ingest/blob/<blake3>/part/<blake3>)
![](/data/notion/ingest/blob/<blake3>)
```

`/data/<group>/<function>/blob/<hash>` is a file the source's download
kept, by the hash of its bytes; `…/part/<hash>` is one part of an email
kept whole. Both are paths on `datalib-http`'s origin, behind the same
API token as the applets (`Authorization: Bearer …`). The answer never
changes, so cache it for good. The server answers any type that could
run script as `application/octet-stream`, and an unknown address as
404.

Any other link in a body is what the source wrote, and points wherever
its sender pointed it. Datalib's viewer loads no remote image until its
reader asks.

## Other links datalib writes

A document about the library itself (the storage report) and the app's
own grids name a source group as `datalib:group/<id>` and a step as
`datalib:step/<group>/<function>`; the contacts app names one of its
contacts as `datalib:contact/<id>`. Each is the same link shape as a
person's. A viewer that does not know one should show its text and not
follow it.
