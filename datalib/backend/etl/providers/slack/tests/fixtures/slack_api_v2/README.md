# slack_api_v2 — the workspace one sync later

The same captured API as `../slack_api/`, as the *second* sync sees it:
`auth.test`, `users.list` and `conversations.list` unchanged, and one
`conversations.history` answer per channel at the `oldest` the first
sync's resume scan computes (its latest `ts`, not inclusive). Every
channel but `#bridge` answers with nothing new. `#bridge` carries the
delta: Riker's "Shields up. Red alert." is **added**; Worf's "I
recommend raising shields" is **edited** (an `edited` stamp, a word
changed, a `:shield:` reaction from Picard); and the "status report"
thread root comes back with `reply_count` advanced, so the sync
re-walks it through `conversations.replies`, whose tape here has the
first sync's three messages plus a fourth from Worf.

What a re-sync cannot carry is a deletion — an incremental
`conversations.history` returns only what is newer than `oldest` — so
that fate is exercised by the contacts fixture instead.

The fixture pipeline (`tests/fixtures/run_sync_pipeline.py`)
synthesizes this into its own playback tree and syncs `slack` against
it after the first pipeline run, then renders a `slack-diff` group
between the two raw commits.
