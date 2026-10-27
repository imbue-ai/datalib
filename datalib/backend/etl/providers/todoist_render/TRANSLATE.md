# Todoist render

The raw store keeps each Todoist object as the API sent it
([`../todoist/INGEST.md`](../todoist/INGEST.md)). This crate turns each
**project** into one markdown document, at
`<source>/render_markdown/projects/<project id>.md`, and gives the
project, each task and each comment a grid row. Every table is read
whole at the render's pin (`src/store.rs`); an account's tasks are few
enough that reading them all costs less than working out which to read.

## What becomes what

`src/render.rs`, top to bottom:

- **The project**: its name as the title, linking to
  `https://app.todoist.com/app/project/<id>`, then its description and
  whether it is archived. Its comments (`project_notes`) come next,
  under "Comments on the project".
- **Sections**, in `section_order`, each a `##` heading. Tasks in no
  section come first, under no heading; so do tasks whose section was
  deleted.
- **Open tasks** in each section as a tree: a task, then its comments,
  then its subtasks one heading level down (`###` for a top-level
  task, to `######`), siblings in `child_order`. A task whose parent is
  completed, deleted or elsewhere is a top-level one. Each task is
  `☐` and its text, then one line: its priority as the app shows it
  (the API's 4 is `p1`), its due date (and the words of a recurring
  one), its deadline and duration, its labels as `@name`, and who it is
  assigned to as a chip; then its description.
- **Comments** follow their task, each with the message header every
  renderer uses: who wrote it, as a chip where their email is known,
  and when. A file attachment is a link to Todoist's URL for it.
- **Completed tasks** of each section, newest first, under a collapsed
  `<details>` ("✓ 3 completed"), each `☑` with when it was done.

Each task and comment is a section with its own
`data-section-uuid`, so a grid row opens at its place in the page.

Task text, descriptions and comments are Todoist markdown, so their
links and emphasis stay; HTML in them is escaped
(`escape_html_outside_code`), since Todoist shows it as text. A
project's and a section's names are plain text and escaped as such.

### Which row says what about a task

A task can be in `items` (from `/sync`), in `completed_items` (from
the completed-tasks endpoint), or both. The `items` row wins when there
is one: it is the newer word, and it says when a completed task was
opened again (`checked: false`). A task is completed when its `items`
row says `checked`, or when only the completed list has it. A task
either row marks `is_deleted` is left out. A task is filed under the
project its winning row names, so a task moved since it was completed
renders once, where it is now.

### Why completed tasks render

They are rendered, folded under each section. A person searching their
tasks is as often looking for when they did something as for what is
left to do, and the completed tasks are most of an account's history:
leaving them out of the documents would leave them out of search, since
search reads the documents. They are folded so the open tasks still
read as the list they are in the app. `completed = false` in the
download's config stops them being fetched; any already stored still
render.

### A project the store does not hold

The completed list can name a project the sync never sent: an archived
one, whose row a full sync leaves out. Its tasks get a document of
their own, titled `Project <id>`, saying the project was not sent. A
project Todoist deleted (`is_deleted`) renders nothing, and its old
document goes (`exclude_bucket`).

## Grid rows

| kind | one per | `created_at` | `modified_at` | `channel` | `item_count` |
|---|---|---|---|---|---|
| Todoist Project | project; the document row | the earliest stamp in the document | the latest | — | its tasks and comments |
| Todoist Task | task, open or completed | `added_at` | `updated_at`, when it differs from `added_at` | the section | 1 |
| Todoist Comment | comment on a task or a project | `posted_at` | — | the task's section | 1 |

`account` is the account's email from the `user` row. A task's author
is whoever added it (`added_by_uid`, else the task's `user_id`); a
comment's is `posted_uid`. Either is named from the `user` row or the
`collaborators`, with their email as the handle; a user neither names
is "Todoist user <id>". A task's row links to
`https://app.todoist.com/app/task/<id>`, the form the reference gives;
a comment's links to its task, or to its project.

Ids are minted through `datalib_id` under the `todoist` namespace
(`src/ids.rs`): a project by its id alone, with no stamp, so a
download problem naming the project finds its document; a task by its
id and `added_at`; a comment by its id and `posted_at`, task and
project comments as two kinds.

## Incrementality

The bucket is the project, keyed by its id. Each bucket declares every
row it read: the project, its sections, tasks, comments, the
collaborators it looked up (found or not) and the account. A changed
row names a project through the store as it is now (`Store::project_of`):
a section or a task by its `project_id`, a comment by its task's. A
task moved from one project to another re-renders both: the new one by
that forward scan, the old one because it declared the task's row.

## Not verified against the live API

The render reads the fields the reference names. None of these was
seen on a real account:

- that a completed task in `/sync`'s `items` keeps its `project_id` and
  `section_id`;
- the project URL form `app.todoist.com/app/project/<id>` (only the task
  form is in the reference);
- that `posted_uid` on a project comment is sometimes a number, as the
  reference's table says; both are read.
