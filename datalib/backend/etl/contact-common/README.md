# contact-common — one page for every source about people

The address book, LinkedIn's connections and Facebook's friends each
hand this crate a `NormalizedContact` per person (in a `ContactDoc`) and
get back a rendered `.md` and the person's grid row. Who a person is,
and what a handle is, is [`docs/dev/contacts.md`](../../../../docs/dev/contacts.md).

## The page

```markdown
<div class="contact-head" data-page-title-uuid="…">

<div class="contact-photo">

![Jean-Luc Picard](blobs/….jpg)

</div>

<div class="contact-who">

# [Jean-Luc Picard](mailto:picard@enterprise.starfleet "Jean-Luc Picard <picard@enterprise.starfleet>")

<p class="contact-role">Captain · Starfleet</p>

<p class="contact-groups"><span class="contact-group">Senior Staff</span></p>

</div>
</div>

<dl class="contact-fields">
<dt>Email <span class="contact-qualifier">work</span></dt><dd class="chip-identifiers">

[picard@enterprise.starfleet](mailto:picard@enterprise.starfleet "picard@enterprise.starfleet")

</dd>
<dt>Address <span class="contact-qualifier">home</span></dt><dd>…</dd>
</dl>
```

- **The heading is the person's chip.** It is a markdown `#` holding a
  chip link, so the viewer draws it as the chip every document draws
  for this person, and its right-click is where a card is linked to
  one of your contacts. The handle is the one the grid's Contact cell
  draws (the first address, else the first full number), else the
  card's first handle of any kind; a card with none gets its name as
  plain text. The copy-page-ID button finds the heading through the
  wrapper's `data-page-title-uuid`.
- **The fields** (`field_rows`): how to reach the person, then
  whatever else the source says, then the note. A plain value is
  escaped as text between tags and its line breaks become `<br>`, so
  nothing in it can end the HTML block. A value that is a handle is a
  chip link, which only markdown makes, so its `<dd>` closes the block
  around one paragraph holding the link. The `chip-identifiers` class
  has the viewer draw it as the address or number rather than the
  person (`decorateHandles`), and a plain click on it opens the link
  popover, never the mail app. Never write a handle as a plain
  `<a href="mailto:…">`: a click on that leaves the app.
- **A group lists its members** under `## Members`, each a chip where
  the member has a handle.
- **The photo is markdown** (`![…](blobs/…)`), not an `<img>`, because
  the viewer rewrites only markdown images to the asset route.

The look is `documentBody.css` (`.contact-head`, `.contact-fields`,
`.contact-members`); a chip's grey (not linked to a contact) and blue
(linked) are `chip.css`. A new class or tag has to pass the sanitizer
(`ui/src/cards/sanitize.ts`).

A change to what this crate writes bumps the `RENDER_VERSION` of every
provider that calls it: `contacts`, `linkedin` and `facebook`.

## Looking at the output

```sh
bazelisk run //datalib/ui:render_preview     # rewrite both goldens
open datalib/ui/tests/goldens/contact_preview.html
```

`contact_preview.html` is every contact page the TNG fixture renders,
then `src/samples.rs`: a card with every field, a minimal one, one with
no handle, a group, a LinkedIn-style connection and one whose every
field is markup. Nothing resolves a chip in the preview, so they draw
grey; the toolbar's "Draw chips as linked" shows the blue.
`//datalib/ui:render_preview_test` fails when the page is stale.
