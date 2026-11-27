//! Placeholder, empty. A playback roundtrip would synth checked-in
//! JSON fixtures → playback dir, point `DATALIB_HTTP_PLAYBACK` at
//! it, run `ingest::fetch`, then assert the resulting doltlite db
//! mirrors the fixture. No `synthesize.rs` exists yet to do the first.

#[test]
fn placeholder() {
    // Intentionally empty — the rule exists in BUILD.bazel so the
    // test wiring is in place for a roundtrip.
}
