//! Live JMAP test (Fastmail). Hits api.fastmail.com via `latchkey`.
//! Tagged `manual` + `external` + `no-sandbox` so it stays out of
//! `bazelisk test //...`. Run with:
//!
//! ```sh
//! bazelisk test //frankweiler/backend/etl/providers/jmap:jmap_live \
//!     --test_arg=--ignored --test_env=PATH --test_env=HOME --test_env=USER
//! ```
//!
//! Stub today — the real golden snapshot lands with the synth + fixture
//! work.

#[test]
#[ignore]
fn live_jmap_fastmail() {
    // TODO: bazelisk run --test_env=... to exercise.
}
