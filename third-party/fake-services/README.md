# fake-services

Fake Slack, Linear and Google APIs (<https://github.com/imbue-ai/fake-services>,
MIT), seeded from JSON files, that the downloader tests under
`datalib/backend/etl/fake_services/` run the real `slack`, `linear`, `email`
(Gmail) and `calendar` (Google) ingests against, through the real latchkey
and the fake's own `curl_shim.sh` in `LATCHKEY_CURL`.

Two pins, bumped together:

- `MODULE.bazel`'s `fake_services` archive is the launcher's source at one
  commit of that repository (`fake_services.BUILD.bazel` exposes what the
  tests stage).
- `runtime/package.json` and its lockfile are the npm packages the launcher
  imports, at the versions the repository's own `package.json` names at that
  commit, built the way `third-party/latchkey/runtime/` builds latchkey's
  tree: integrity-checked, no install script runs. Refresh the lockfile
  after editing `package.json` with
  `bazel run -- @pnpm --dir third-party/fake-services/runtime install --lockfile-only`.
