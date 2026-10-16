//! Encode every series `scripts/fit_time_series_export.py` wrote at several chunk sizes and
//! print what each costs: blob bytes, the store on disk after `dolt_gc`,
//! and decode time. README.md says how to run it and what it found.
//!
//!     measure <series.jsonl> <scratch dir>

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::{self, StoreKind};
use datalib_etl_time_series::chunk::{self, Compression};
use datalib_etl_time_series::store;
use datalib_time_series_schema::{Points, TimeSeriesChunkRow};
use serde::Deserialize;

#[derive(Deserialize)]
struct Exported {
    field: u32,
    t: Vec<i64>,
    raw: Vec<i64>,
    scale: f64,
    offset: f64,
    semicircles: bool,
}

impl Exported {
    fn integers(&self) -> Points {
        Points::integers(self.t.iter().copied().zip(self.raw.iter().copied()))
    }

    /// The values in their unit, as a render would store them: the
    /// offset taken off before the division, so a reading with one
    /// decimal stays one (`2501 / 5 - 500` is 0.19999999999998863).
    fn floats(&self) -> Points {
        Points::floats(self.t.iter().copied().zip(self.raw.iter().map(|&r| {
            if self.semicircles {
                r as f64 * 180.0 / 2f64.powi(31)
            } else {
                (r as f64 - self.offset * self.scale) / self.scale
            }
        })))
    }
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)? {
        n += e?.metadata()?.len();
    }
    Ok(n)
}

async fn on_disk(chunks: &[TimeSeriesChunkRow], dir: &Path) -> Result<u64> {
    let _ = std::fs::remove_dir_all(dir);
    let pool = store::open(&dir.join(store::DB), StoreKind::TimeSeries).await?;
    let mut tx = pool.begin().await?;
    for c in chunks {
        store::write_chunk(&mut tx, c).await?;
    }
    tx.commit().await?;
    doltlite_raw::commit_run(&pool, "measure").await?;
    sqlx::query("SELECT dolt_gc()").fetch_all(&pool).await?;
    pool.close().await;
    dir_size(dir)
}

// A measurement prints its table; there is no log to send it to.
#[allow(clippy::disallowed_macros)]
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (input, scratch) = (Path::new(&args[1]), Path::new(&args[2]));
    let all: Vec<Exported> = std::fs::read_to_string(input)
        .context("read the export")?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let values: usize = all.iter().map(|s| s.raw.len()).sum();
    let degrees: usize = all
        .iter()
        .filter(|s| s.semicircles)
        .map(|s| s.raw.len())
        .sum();
    println!(
        "{} series, {values} values, {degrees} of them latitude or longitude",
        all.len()
    );
    println!();
    println!("| points a chunk | as | deflate | blob B/value | lat/long B/value | rest B/value | on disk B/value | decode ns/value |");
    println!("|---|---|---|---|---|---|---|---|");
    for size in [256, 1_024, 4_096] {
        for (as_what, floats) in [("integers", false), ("floats", true)] {
            for compression in [Compression::Never, Compression::WhenSmaller] {
                let (mut total, mut deg) = (0usize, 0usize);
                let mut chunks = Vec::new();
                for (key, s) in all.iter().enumerate() {
                    let p = if floats { s.floats() } else { s.integers() };
                    if let Some(problem) = p.problem() {
                        anyhow::bail!("field {}: {problem}", s.field);
                    }
                    for mut c in chunk::cut(key as i64 + 1, &p, size) {
                        let part = chunk::decode(c.encoding, &c.points)?;
                        c.points = chunk::encode(&part, compression);
                        total += c.points.len();
                        if s.semicircles {
                            deg += c.points.len();
                        }
                        chunks.push(c);
                    }
                }
                let start = Instant::now();
                let mut decoded = 0;
                for c in &chunks {
                    decoded += chunk::decode(c.encoding, &c.points)?.len();
                }
                let ns = start.elapsed().as_nanos() as f64 / decoded as f64;
                let disk = if compression == Compression::WhenSmaller {
                    let d = on_disk(&chunks, &scratch.join(format!("{size}-{as_what}"))).await?;
                    format!("{:.2}", d as f64 / values as f64)
                } else {
                    "".into()
                };
                println!(
                    "| {size} | {as_what} | {} | {:.2} | {:.2} | {:.2} | {disk} | {ns:.1} |",
                    if compression == Compression::Never {
                        "no"
                    } else {
                        "when smaller"
                    },
                    total as f64 / values as f64,
                    deg as f64 / degrees as f64,
                    (total - deg) as f64 / (values - degrees) as f64,
                );
            }
        }
    }
    Ok(())
}
