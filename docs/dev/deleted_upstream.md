# Keeping what you deleted upstream

By default a sync copies deletions: a conversation you delete on
claude.ai disappears from your datalib copy on the next sync (it stays
in the store's doltlite history, but no longer renders or shows in
search). A source that offers `when_deleted_upstream` lets you choose
the other way:

```toml
[steps.params]
when_deleted_upstream = "keep_local_copy"   # default: "delete_local_copy"
```

With `"keep_local_copy"` the copy here stays as it was last fetched,
and keeps rendering and showing in search, after the record is deleted
upstream.

## Why someone wants this

Deleting your data from a service is a reasonable privacy step, and
often the only one you have. What you leave with a company outlives
your relationship with it: the company can be sold, or go bankrupt, and
its user data can go to whoever buys its assets. When Character.AI's
maker filed for bankruptcy in 2026, nobody could say whether its users'
chats were among the assets for sale, and 23andMe's customers' genetic
data had already been sold in that company's bankruptcy
([KQED, "Your Chatbot Just Went Bankrupt. What Happens Now to Your
User Data?", Rachael Myrow, October 2026](https://www.kqed.org/news/12104362/your-chatbot-just-went-bankrupt-what-happens-now-to-your-user-data)).

So a person may want to delete their conversations from claude.ai or
chatgpt.com once they trust their local copy, and keep that copy. With
the default, the next sync would delete the local copy too. With
`"keep_local_copy"` it does not.

Check the copy first: open a few long conversations in the app, with
their attachments, before deleting anything upstream. Once a record is
gone upstream, the local copy is the only one, and it cannot be fetched
again.

## What it covers

- **A record upstream no longer lists stays.** That is a conversation a
  complete listing leaves out, or one a newer export no longer holds.
  The run's summary counts them every run (`kept_deleted_upstream=N`),
  and the log has a `kept_deleted_upstream` line naming the collection.
- **An edit upstream still replaces the copy.** A conversation that is
  still there but changed is fetched again and written over the old
  copy, as always; the old version is in the store's history.
- **Bookkeeping with nothing in it still goes.** Claude's stubs (a
  conversation every fetch of which failed, so we hold no content) are
  deleted when no listing names them, whatever this says.
- **Turning it back to `"delete_local_copy"`** deletes, on the next
  complete listing, everything that was kept.

## Which sources offer it

`claude` and `chatgpt`, for both the `api` and `export` methods. Any
other source refuses the key at load, so it cannot be set somewhere it
would be silently ignored.

## Adding it to another source

1. Add `when_deleted_upstream: WhenDeletedUpstream` (from
   `datalib_source_common`, `#[serde(default)]`) to the provider's
   config struct, and carry it to the download.
2. Where the download deletes what upstream stopped listing, call
   `datalib_etl::prune::follow_deletions_in_tx` instead of
   `prune_scope_in_tx`. It answers what it `deleted` (delete those
   records' edges, as before) and what it `kept` (count them in the run
   summary and pass the count to `prune::record_kept`).
3. Add a test that a record upstream drops is kept, and that switching
   back deletes it.
4. Name the source in the list above, and offer the choice in its
   wizard entry (`datalib/ui/src/config/catalog.ts`).
