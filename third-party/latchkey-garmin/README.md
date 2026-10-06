# latchkey-garmin, vendored

The built files of [imbue-ai/latchkey-garmin](https://github.com/imbue-ai/latchkey-garmin)
(MIT, see `LICENSE`), copied unchanged from commit
`0928ac064ab8ad33bb365ed492556cc72b736aec`: the latchkey plugin that adds a `garmin` service. `datalib-http`
embeds them and installs them into latchkey's plugins directory the first
time someone signs in to Garmin (`datalib/backend/http/src/plugins.rs`).

Only what latchkey loads is here: `dist/` and `package.json`. To move to a
newer commit, copy those two from a checkout of it, put its commit above,
and bump `GARMIN.version` in `plugins.rs` so installed copies are
replaced.
