* **Before open-sourcing: confirm the purged manual-e2e data is gone server-side.**
  The history rewrite itself is DONE. `configs/thad_tiny.yaml` and the 468
  `.snap` files under the old `backend/sync/tests/snapshots/` left the working
  tree in 26412853 (they live in the private `data_liberation_manual_e2e_test_data`
  dir) and were later expunged from history with `git filter-repo` — no reachable
  commit on `main` or `origin/main` contains either path. Two residual risks
  remain before the repo is made public: GitHub still holds the pre-rewrite
  blobs as unreachable objects, addressable by SHA until a support request
  GCs them; and any collaborator who never re-cloned still has them locally.
* Notion: The order of the blocks in this markdown looks wrong: /Users/thad/datalib.thad_tiny_1/rendered_md/notion/pages/364a550f-af95-80de-829f-c5fccb3021fd/index.md
* Make sure that markdown for Notion and Slack has relative links for other documents and media.