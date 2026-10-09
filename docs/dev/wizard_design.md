# How a source wizard is designed

The "Add source" dialog is filled in by people who do not know what an
ingest step is. This is how a form for them is put together. The forms
in [`catalog.ts`](../../datalib/ui/src/config/catalog.ts) are the
worked examples: Slack for a connected account, WhatsApp for a backup
that needs a secret, Apple Messages for a folder macOS guards. Read one
of those beside this before adding or changing an entry.

## The shape every form has

1. **Name**, first, optional.
2. **The account**, for a source that signs in. Once a check reaches
   the account the row says who it is, and everything about signing in
   sits behind "Use a different account".
3. **The source's own questions**: the entry's `sections`, a heading in
   the left column and its controls in the right.
4. **Advanced options**, closed. It holds the ID first, then the
   entry's advanced fields, then Rendering, Description and the TOML.

`SourceWizard.vue` draws all of it from the entry. An entry declares
`fields` (what is written) and `sections` (how it is asked); a field no
section names is drawn in Advanced options under its own label.

## What goes in the basic part

Only what a person must decide to get the data they came for: which
account, which folder, which channels, how far back. Everything else is
advanced. Tuning (a concurrency, a refresh window), storage-engine
switches, and anything named after a step or a table are advanced even
when they are important.

- **Ask a question, offer answers.** A setting that means "everything,
  unless a list is given" is a question with two answers, not an empty
  box with "leave empty for all" beside it. Two fields that interact
  (Slack's channel list and its "channels you're not in" switch) are
  one question. Write them as a section's `answers`: an answer may
  `sets` switches and show `fields` while it is chosen.
- **The default answer is first, and adding with every default must
  work.** A form a person can finish by choosing a folder and pressing
  Add is the goal.
- **Say what is needed before the form, not inside a field's help.** A
  source that needs something fetched from a phone or set up in a
  terminal says so in `before`.
- **A path is chosen, not typed** ([`wizard_file_pickers.md`](wizard_file_pickers.md)).
- **A secret is never typed into the form.** It is named, by an
  environment variable (`envVar`) or a latchkey account.

## Wording

- **Basic part: the person's words.** "Copy", not "mirror"; "sync", not
  "run"; "folder", not "directory". No step ids, no "render", no
  "latchkey service". A heading is two or three words or a short
  question. Help is one or two sentences and says a consequence the
  person would otherwise be surprised by; if it says nothing like that,
  leave it out.
- **Advanced options: the accurate term, briefly.** The reader is
  technical, so name the step, the table, the SQL statement. State what
  the setting does and what its empty value means, and stop. Use the
  same "copy" and "sync" as the basic part.
- **One word for one thing across the whole dialog.**

## Keeping a form honest

- **Every field an entry declares is drawn.** A setting not worth a
  question goes in Advanced options; it is never left off the form.
- **No control that cannot work.** A check the backend cannot make, or
  a sign-in the service does not offer, is not drawn.
- **Editing reopens the same form.** An answer is read back from the
  values (`chosenAnswer` in `sourceSteps.ts`), so every answer must be
  recognizable from what it wrote.
