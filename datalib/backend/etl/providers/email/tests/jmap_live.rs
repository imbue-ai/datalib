//! Live JMAP test (Fastmail). Hits api.fastmail.com via `latchkey`.
//! Tagged `manual` + `external` + `no-sandbox` so it stays out of
//! `bazelisk test //...`. Run with:

#[test]
#[ignore]
fn live_jmap_fastmail() {
    // TODO: bazelisk run --test_env=... to exercise.
}
