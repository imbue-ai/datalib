# Google Drive render

The raw store keeps each file in Google's own JSON at every version a
sync saw ([`../google_drive/INGEST.md`](../google_drive/INGEST.md)).
This crate turns each **Google Doc** into one markdown document, from its
newest capture. Sheets and Slides do not render yet; they stay in the
raw store.

## What becomes what

`src/doc.rs` reads the Docs API's `documents.get` answer, every tab,
suggestions inline:

- **Headings.** The `TITLE` and `HEADING_1` styles are `#`, and
  `HEADING_2` to `HEADING_6` are `##` to `######`. A `SUBTITLE` is an
  italic line. A Doc with more than one tab gets a `#` heading per tab,
  named for it; child tabs follow their parent.
- **Text.** Bold, italic, strikethrough, superscript, subscript and
  links stay. A run set in a monospace font (Courier New, Roboto Mono,
  Consolas and the like) is inline code, and a run of paragraphs set
  wholly in one is a fenced code block. Docs marks its own code block
  building block with the character U+E907, which is dropped. Fonts,
  sizes, colours and underline are left out: markdown has no place for
  them.
- **Lists.** Each bullet paragraph is a list item, indented four spaces
  per nesting level. Its list's glyph says numbered (decimal, letters,
  roman) or not.
- **Tables** are markdown tables, the first row as the header. A cell
  with several paragraphs joins them with `<br>`, a bullet in a cell is
  `•`, and a table inside a cell is its text on one line.
- **Images** are `![title](blobs/<file>)`, the bytes the download stored
  for that image written beside the document. The alt text is the
  image's title, else its description. An image whose bytes the store
  does not hold reads `(image not downloaded)` and is a `blob_missing`
  problem on the document.
- **Chips.** A person chip is a chip link to their address
  (`mailto:`), drawn as a contact like any author. A rich link (a Doc,
  a Sheet, a video) is a link to it, named by its title. A date chip and
  a dropdown are the text they show.
- **Footnotes** are `<sup>n</sup>` where they are cited, and their text
  after the tab, each after its number.
- **Suggestions.** Text someone suggested adding is `<ins>`, text
  suggested for removal `<del>`, so the document reads as it does in
  Docs' suggesting mode.
- **Left out on purpose:** headers and footers, page and column breaks,
  page numbers and section breaks (page layout, in a document with no
  pages), and a table of contents (the headings again). An equation is
  `(equation)`: the API says where one is and nothing of what it says.

## What it does not know

Anything the converter does not recognise (a structural element, a
paragraph element, an embedded object that is not an image) is kept, not
dropped. It shows where it was as a collapsed
`<details class="unsupported-json">` holding the JSON Google sent, and
the document gets one `unsupported_json` warning per kind of thing, at
the first place it was seen. A new kind of element Google adds therefore
reaches the reader and the Manage row's problem count.

On the account's 280 Docs, measured on 2026-10-21, nothing fell through
to this.

## Comment threads

`src/comments.rs`. Each thread is a collapsed
`<details class="comment-thread">`. Its summary says how many messages
it holds, whether it was resolved, and the start of the text it was made
on. Inside are that text as a quote, then the comment and every reply.
Each message is a `<div class="msg">` section with the header every chat
uses: author, then time. A reply that resolved or reopened the thread
says so, and a deleted one says it was deleted.

**Where a thread goes.** Drive's `anchor` is an id only the Docs editor
can resolve, but a comment carries the text that was selected
(`quotedFileContent`, HTML-escaped). A thread follows the first block
holding that text's first line. When the text has been edited since, it
follows the block sharing the longest opening of it, at least 24
characters. A thread on a list item waits for the end of the list, so
the list stays one list. A thread with no quote, or whose text is gone,
goes under "Comments not tied to the text" at the end. Measured on the
account above: 2,311 of 2,966 threads placed, 79 with no quote, the rest
on text since deleted or rewritten.

**Who wrote it.** Drive names a comment's author by display name only:
no address. The author's handle is `google:name/<the name>`, which the
contacts app can link to a person. The account's own comments use its
address, where the file names the account among its owners, last editor
or sharer.

## Rows

| row | kind | `created_at` | author |
|---|---|---|---|
| the Doc | `Google Doc` | the file's `createdTime` | its first owner, by address |
| each comment and reply | `Google Doc Comment` | its own `createdTime` | as above |

Each row's `source_url` opens the Doc in Docs; a comment's opens that
comment (`?disco=<comment id>`). The document's id is minted from the
file id alone, so a fetch problem keyed `files:<id>` lands on it.

**A bucket is a file.** It reads its `files` row, its newest `captures`
row, that capture's `capture_parts` and its `comments`. Every key in
those tables starts with the file id, which is how the forward scan maps
a changed row to the Doc to render again.
