//! Parse real CSV captures from `us.yosmart.com/download/...`.
//! Gated by `YOLINK_FIXTURE_DIR` (captures embed bearer tokens
//! and per-account device IDs; they don't belong in the repo).
//!
//! Expects `bfreezer1.out` (THSensor) and `valve.out`
//! (WaterMeter) under the fixture dir.

use std::{env, fs, path::PathBuf};

use frankweiler_etl_yolink::extract::parse;

fn dir() -> Option<PathBuf> {
    env::var_os("YOLINK_FIXTURE_DIR").map(PathBuf::from)
}

#[test]
fn temperature_humidity() {
    let Some(d) = dir() else { return };
    let rows = parse(&fs::read_to_string(d.join("bfreezer1.out")).unwrap(), "temperature_humidity").unwrap();
    assert!(rows.len() > 100);
}

#[test]
fn watermeter_is_monotonic() {
    let Some(d) = dir() else { return };
    let rows = parse(&fs::read_to_string(d.join("valve.out")).unwrap(), "watermeter").unwrap();
    let mut prev = 0.0f64;
    for r in rows.iter().filter(|r| r.metric == "water_meter_gal") {
        assert!(r.value + 1e-6 >= prev, "meter went backwards at {}", r.ts_ms);
        prev = r.value;
    }
}
