//! Placeholder. The real playback roundtrip will: synth checked-in
//! JSON fixtures → playback dir, point `FRANKWEILER_HTTP_PLAYBACK` at
//! it, run `download::fetch`, then assert the resulting doltlite db
//! mirrors the fixture. Lands with the `synthesize.rs` implementation.

#[test]
fn placeholder() {
    // Intentionally empty — the rule exists in BUILD.bazel so the
    // test wiring is in place when synth + playback roundtrip lands.
}
