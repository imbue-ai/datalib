* **Before open-sourcing: expunge the manual-e2e test data from git HISTORY.**
  We moved `configs/thad_tiny.yaml` and `frankweiler/backend/sync/tests/snapshots/`
  out of the repo (into the private `data_liberation_manual_e2e_test_data` dir)
  and deleted them from the working tree, but they still live in past commits —
  the slightly sensitive source data (contacts, LinkedIn, SMS, Takeout snapshots)
  is recoverable from history until purged. Rewrite history to remove them
  (e.g. `git filter-repo --path configs/thad_tiny.yaml --path
  frankweiler/backend/sync/tests/snapshots --invert-paths`), force-push, and have
  collaborators re-clone. Do this before the repo is ever made public.
* Notion: The order of the blocks in this markdown looks wrong: /Users/thad/datalib.thad_tiny_1/rendered_md/notion/pages/364a550f-af95-80de-829f-c5fccb3021fd/index.md
* Make sure that markdown for Notion and Slack has relative links for other documents and media.