//! A 2-D map of document embeddings: nearest neighbours, a starting
//! layout seeded from the previous map, UMAP, and a fit back onto the
//! previous map's frame. Pure computation over values; the step that
//! reads qmd's vectors and writes the map is `datalib_step::embedding_map`.
//! The README beside this file says why each stage is there.

use std::collections::HashMap;

use ndarray::ArrayView2;
use rayon::prelude::*;
use umap_rs::{GraphParams, Umap, UmapConfig};

pub type Point = [f32; 2];

/// UMAP's usual neighbourhood, the point itself included. A corpus
/// this small or smaller uses all of itself but one.
pub const N_NEIGHBORS: usize = 15;
/// Epochs for a layout started from nothing, as umap-learn picks them.
pub const FRESH_EPOCHS_SMALL: usize = 500;
pub const FRESH_EPOCHS_LARGE: usize = 200;
/// Epochs and step size for a layout started from the previous map. The
/// neighbourhoods are already where they belong, so the run only has to
/// settle what moved; a full-strength run would shuffle what did not.
pub const SEEDED_EPOCHS: usize = 200;
pub const SEEDED_LEARNING_RATE: f32 = 0.1;

/// One vector per document, every row unit length.
pub struct Corpus {
    pub keys: Vec<String>,
    pub dim: usize,
    pub vectors: Vec<f32>,
}

impl Corpus {
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn row(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

/// How each point got its starting position.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SeedStats {
    /// Where the previous map had it.
    pub kept: usize,
    /// New, placed among its neighbours from the previous map.
    pub near_neighbours: usize,
    /// Placed by the corpus's principal axes: every point on a first run,
    /// and a new point none of whose neighbours were mapped before.
    pub fresh: usize,
}

impl SeedStats {
    pub fn seeded(&self) -> bool {
        self.kept > 0
    }
}

pub struct Layout {
    pub points: Vec<Point>,
    pub seed: SeedStats,
    pub epochs: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Neighbours,
    Manifold,
    Epoch { done: usize, total: usize },
}

/// Scale `v` to unit length. False, and `v` untouched, for a zero vector.
pub fn unit(v: &mut [f32]) -> bool {
    let norm = dot(v, v).sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return false;
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    true
}

/// Eight running sums rather than one, so the compiler can vectorise:
/// a single `sum()` fixes the order of every addition and cannot.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for l in 0..8 {
            acc[l] += x[l] * y[l];
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// Each point's `k` nearest by cosine distance, row-major `n × k`, the
/// point itself first — UMAP's convention, which its local-connectivity
/// step relies on. Exact, by brute force: `O(n² · dim)` as matrix
/// products, split over every core.
pub struct Knn {
    pub k: usize,
    pub indices: Vec<u32>,
    pub dists: Vec<f32>,
}

impl Knn {
    pub fn neighbours(&self, i: usize) -> &[u32] {
        &self.indices[i * self.k..(i + 1) * self.k]
    }
}

pub fn nearest_neighbours(corpus: &Corpus, k: usize) -> Knn {
    // Rows a block: one matrix product per block, `BLOCK × n` similarities
    // at a time, which is what makes brute force affordable.
    const BLOCK: usize = 256;
    let n = corpus.len();
    let all = ArrayView2::from_shape((n, corpus.dim), &corpus.vectors)
        .expect("a corpus's vectors are n × dim");
    let by = |x: &(f32, u32), y: &(f32, u32)| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1));
    let blocks: Vec<Vec<(f32, u32)>> = (0..n.div_ceil(BLOCK))
        .into_par_iter()
        .map(|b| {
            let rows = b * BLOCK..((b + 1) * BLOCK).min(n);
            let sims = all.slice(ndarray::s![rows.clone(), ..]).dot(&all.t());
            let mut out = Vec::with_capacity(rows.len() * k);
            let mut scratch: Vec<(f32, u32)> = Vec::with_capacity(n);
            for (r, i) in rows.enumerate() {
                scratch.clear();
                // The point itself sorts ahead of an exact duplicate.
                scratch.extend(sims.row(r).iter().enumerate().map(|(j, &sim)| {
                    let d = if i == j { -1.0 } else { (1.0 - sim).max(0.0) };
                    (d, j as u32)
                }));
                if k < n {
                    scratch.select_nth_unstable_by(k, by);
                    scratch.truncate(k);
                }
                scratch.sort_unstable_by(by);
                out.extend_from_slice(&scratch);
            }
            out
        })
        .collect();
    let mut indices = Vec::with_capacity(n * k);
    let mut dists = Vec::with_capacity(n * k);
    for (d, j) in blocks.into_iter().flatten() {
        indices.push(j);
        dists.push(d.max(0.0));
    }
    Knn { k, indices, dists }
}

/// The corpus projected onto its first two principal axes, scaled so
/// the wider one spans `[-5, 5]` — the ten units umap-learn starts a
/// layout in. Power iteration from a fixed start,
/// so the same corpus always lands the same way up.
pub fn principal_axes(corpus: &Corpus) -> Vec<Point> {
    let (n, dim) = (corpus.len(), corpus.dim);
    if n == 0 {
        return Vec::new();
    }
    let mut mean = vec![0f32; dim];
    for i in 0..n {
        for (m, x) in mean.iter_mut().zip(corpus.row(i)) {
            *m += x;
        }
    }
    for m in mean.iter_mut() {
        *m /= n as f32;
    }
    let centered = |i: usize, out: &mut [f32]| {
        for ((o, x), m) in out.iter_mut().zip(corpus.row(i)).zip(&mean) {
            *o = x - m;
        }
    };
    let mut axes: Vec<Vec<f32>> = Vec::new();
    for axis in 0..2 {
        let mut v: Vec<f32> = (0..dim)
            .map(|j| ((j * 7919 + axis * 104_729) % 13) as f32 - 6.0)
            .collect();
        unit(&mut v);
        let mut row = vec![0f32; dim];
        for _ in 0..60 {
            let mut next = vec![0f32; dim];
            for i in 0..n {
                centered(i, &mut row);
                let proj = dot(&row, &v);
                for (acc, x) in next.iter_mut().zip(&row) {
                    *acc += proj * x;
                }
            }
            for prev in &axes {
                let along = dot(&next, prev);
                for (x, p) in next.iter_mut().zip(prev) {
                    *x -= along * p;
                }
            }
            if !unit(&mut next) {
                break;
            }
            v = next;
        }
        axes.push(v);
    }
    let mut row = vec![0f32; dim];
    let mut points: Vec<Point> = (0..n)
        .map(|i| {
            centered(i, &mut row);
            [dot(&row, &axes[0]), dot(&row, &axes[1])]
        })
        .collect();
    let extent = points
        .iter()
        .flat_map(|p| p.iter())
        .fold(0f32, |m, x| m.max(x.abs()));
    if extent > 0.0 {
        for p in points.iter_mut() {
            p[0] *= 5.0 / extent;
            p[1] *= 5.0 / extent;
        }
    }
    points
}

/// Where each point starts from, given the previous map: its old place
/// if it had one; else the mean of its neighbours' places, spreading
/// outward a ring at a time; else `None`.
pub fn seed_from_prior(
    keys: &[String],
    knn: &Knn,
    prior: &HashMap<String, Point>,
) -> (Vec<Option<Point>>, SeedStats) {
    const RINGS: usize = 4;
    let mut stats = SeedStats::default();
    let mut placed: Vec<Option<Point>> = keys.iter().map(|k| prior.get(k).copied()).collect();
    stats.kept = placed.iter().filter(|p| p.is_some()).count();
    if stats.kept == 0 {
        return (placed, stats);
    }
    for _ in 0..RINGS {
        // Read from the ring before, so the answer does not depend on
        // the order points are visited in.
        let before = placed.clone();
        let mut moved = false;
        for (i, slot) in placed.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            let near: Vec<Point> = knn
                .neighbours(i)
                .iter()
                .filter(|&&j| j as usize != i)
                .filter_map(|&j| before[j as usize])
                .collect();
            if near.is_empty() {
                continue;
            }
            let count = near.len() as f32;
            let sum = near
                .iter()
                .fold([0f32; 2], |s, p| [s[0] + p[0], s[1] + p[1]]);
            *slot = Some([sum[0] / count, sum[1] / count]);
            stats.near_neighbours += 1;
            moved = true;
        }
        if !moved {
            break;
        }
    }
    stats.fresh = placed.iter().filter(|p| p.is_none()).count();
    (placed, stats)
}

/// A similarity transform of the plane: `p ↦ scale · R · (p − from) + to`,
/// `R` a rotation, or a rotation after a flip of the y axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Similarity {
    pub from: Point,
    pub to: Point,
    pub scale: f32,
    pub cos: f32,
    pub sin: f32,
    pub flip: bool,
}

impl Similarity {
    pub fn apply(&self, p: Point) -> Point {
        let x = p[0] - self.from[0];
        let y = if self.flip {
            self.from[1] - p[1]
        } else {
            p[1] - self.from[1]
        };
        [
            self.scale * (self.cos * x - self.sin * y) + self.to[0],
            self.scale * (self.sin * x + self.cos * y) + self.to[1],
        ]
    }
}

/// The similarity transform taking `from` closest to `to` in least
/// squares (orthogonal Procrustes with scale). `None` for fewer than two
/// pairs or a `from` with no spread.
pub fn fit_similarity(from: &[Point], to: &[Point]) -> Option<Similarity> {
    let n = from.len().min(to.len());
    if n < 2 {
        return None;
    }
    let mean = |ps: &[Point]| {
        let s = ps[..n]
            .iter()
            .fold([0f64; 2], |s, p| [s[0] + p[0] as f64, s[1] + p[1] as f64]);
        [s[0] / n as f64, s[1] / n as f64]
    };
    let (mf, mt) = (mean(from), mean(to));
    let best = [false, true]
        .into_iter()
        .map(|flip| {
            let (mut a, mut b, mut norm) = (0f64, 0f64, 0f64);
            for (p, q) in from[..n].iter().zip(&to[..n]) {
                let x = p[0] as f64 - mf[0];
                let y = if flip {
                    mf[1] - p[1] as f64
                } else {
                    p[1] as f64 - mf[1]
                };
                let (u, v) = (q[0] as f64 - mt[0], q[1] as f64 - mt[1]);
                a += x * u + y * v;
                b += x * v - y * u;
                norm += x * x + y * y;
            }
            (flip, a, b, norm)
        })
        .max_by(|l, r| l.1.hypot(l.2).total_cmp(&r.1.hypot(r.2)))?;
    let (flip, a, b, norm) = best;
    if norm == 0.0 {
        return None;
    }
    let len = a.hypot(b);
    let (cos, sin) = if len == 0.0 {
        (1.0, 0.0)
    } else {
        (a / len, b / len)
    };
    Some(Similarity {
        from: [mf[0] as f32, mf[1] as f32],
        to: [mt[0] as f32, mt[1] as f32],
        scale: (len / norm) as f32,
        cos: cos as f32,
        sin: sin as f32,
        flip,
    })
}

/// A small nudge, the same every run for the same key, so points that
/// start on top of each other (duplicates, a new point's neighbours'
/// mean) are not left exactly coincident.
fn nudge(key: &str, radius: f32) -> Point {
    // FNV-1a: stable across builds and platforms, unlike `DefaultHasher`.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let angle = (h & 0xffff) as f32 / 65536.0 * std::f32::consts::TAU;
    let r = radius * (0.5 + ((h >> 16) & 0xffff) as f32 / 131_072.0);
    [r * angle.cos(), r * angle.sin()]
}

fn extent(points: &[Point]) -> f32 {
    let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    for p in points {
        for a in 0..2 {
            lo[a] = lo[a].min(p[a]);
            hi[a] = hi[a].max(p[a]);
        }
    }
    (hi[0] - lo[0]).max(hi[1] - lo[1]).max(0.0)
}

/// The starting layout: the previous map where it reaches, the principal
/// axes — carried into the previous map's frame — where it does not.
pub fn initial_layout(
    corpus: &Corpus,
    knn: &Knn,
    prior: &HashMap<String, Point>,
) -> (Vec<Point>, SeedStats) {
    let (seeded, mut stats) = seed_from_prior(&corpus.keys, knn, prior);
    let needs_axes = seeded.iter().any(Option::is_none);
    let axes = if needs_axes {
        principal_axes(corpus)
    } else {
        Vec::new()
    };
    if !stats.seeded() {
        stats.fresh = corpus.len();
    }
    let into_prior_frame = if stats.seeded() && needs_axes {
        let (from, to): (Vec<Point>, Vec<Point>) = corpus
            .keys
            .iter()
            .zip(&axes)
            .filter_map(|(k, a)| prior.get(k).map(|p| (*a, *p)))
            .unzip();
        fit_similarity(&from, &to)
    } else {
        None
    };
    let mut points: Vec<Point> = seeded
        .iter()
        .enumerate()
        .map(|(i, s)| match (s, into_prior_frame) {
            (Some(p), _) => *p,
            (None, Some(t)) => t.apply(axes[i]),
            (None, None) => axes[i],
        })
        .collect();
    let radius = (extent(&points) * 1e-3).max(1e-4);
    for (p, key) in points.iter_mut().zip(&corpus.keys) {
        let d = nudge(key, radius);
        p[0] += d[0];
        p[1] += d[1];
    }
    (points, stats)
}

/// Run UMAP over `knn` from `init`: `umap-rs` builds the fuzzy graph
/// and fits the curve, and [`optimize`] lays it out.
pub fn umap(
    corpus: &Corpus,
    knn: &Knn,
    init: &[Point],
    epochs: usize,
    learning_rate: f32,
    on_progress: &dyn Fn(Progress),
) -> Vec<Point> {
    let n = corpus.len();
    let config = UmapConfig {
        n_components: 2,
        graph: GraphParams {
            n_neighbors: knn.k,
            ..Default::default()
        },
        ..Default::default()
    };
    let data = ArrayView2::from_shape((n, corpus.dim), &corpus.vectors)
        .expect("a corpus's vectors are n × dim");
    let idx = ArrayView2::from_shape((n, knn.k), &knn.indices).expect("knn is n × k");
    let dists = ArrayView2::from_shape((n, knn.k), &knn.dists).expect("knn is n × k");
    on_progress(Progress::Manifold);
    let manifold = Umap::new(config).learn_manifold(data, idx, dists);
    let graph = manifold.graph();
    let mut edges = Vec::with_capacity(graph.nnz());
    for (row, vec) in graph.outer_iterator().enumerate() {
        for (col, &w) in vec.iter() {
            edges.push((row as u32, col as u32, w));
        }
    }
    let (a, b) = manifold.curve_params();
    let mut points = init.to_vec();
    optimize(
        &mut points,
        &edges,
        Curve { a, b },
        epochs,
        learning_rate,
        on_progress,
    );
    points
}

/// The low-dimensional similarity `1 / (1 + a·d^(2b))`.
#[derive(Debug, Clone, Copy)]
pub struct Curve {
    pub a: f32,
    pub b: f32,
}

/// umap-learn's `optimize_layout_euclidean`, on one thread with a fixed
/// seed. Ours rather than `umap-rs`'s, whose optimiser stops clusters
/// forming: README.md § "Why the optimiser is ours".
pub fn optimize(
    points: &mut [Point],
    edges: &[(u32, u32, f32)],
    curve: Curve,
    epochs: usize,
    learning_rate: f32,
    on_progress: &dyn Fn(Progress),
) {
    const NEGATIVE_SAMPLES: f64 = 5.0;
    const GAMMA: f32 = 1.0;
    let clip = |g: f32| g.clamp(-4.0, 4.0);
    let Curve { a, b } = curve;
    let n = points.len();
    if n == 0 || epochs == 0 {
        return;
    }
    let max_w = edges.iter().fold(0f32, |m, e| m.max(e.2));
    // An edge too weak to be sampled once in the whole run is dropped,
    // as umap-learn drops it.
    let edges: Vec<(usize, usize, f64)> = edges
        .iter()
        .filter(|e| max_w > 0.0 && e.2 >= max_w / epochs as f32)
        .map(|&(h, t, w)| (h as usize, t as usize, (max_w / w) as f64))
        .collect();
    let mut next_sample: Vec<f64> = edges.iter().map(|e| e.2).collect();
    let mut next_negative: Vec<f64> = edges.iter().map(|e| e.2 / NEGATIVE_SAMPLES).collect();
    let mut rng = SplitMix(0x05ee_d0fd_a7a1_1b00);
    for epoch in 0..epochs {
        let alpha = learning_rate * (1.0 - epoch as f32 / epochs as f32);
        let now = epoch as f64;
        for (i, &(j, k, every)) in edges.iter().enumerate() {
            if next_sample[i] > now {
                continue;
            }
            let (dx, dy) = (points[j][0] - points[k][0], points[j][1] - points[k][1]);
            let d2 = dx * dx + dy * dy;
            if d2 > 0.0 {
                let coeff = -2.0 * a * b * d2.powf(b - 1.0) / (a * d2.powf(b) + 1.0);
                let (gx, gy) = (clip(coeff * dx) * alpha, clip(coeff * dy) * alpha);
                points[j][0] += gx;
                points[j][1] += gy;
                points[k][0] -= gx;
                points[k][1] -= gy;
            }
            next_sample[i] += every;
            let every_negative = every / NEGATIVE_SAMPLES;
            let pushes = ((now - next_negative[i]) / every_negative).max(0.0) as usize;
            for _ in 0..pushes {
                let other = rng.below(n);
                if other == j {
                    continue;
                }
                let (dx, dy) = (
                    points[j][0] - points[other][0],
                    points[j][1] - points[other][1],
                );
                let d2 = dx * dx + dy * dy;
                if d2 <= 0.0 {
                    continue;
                }
                let coeff = 2.0 * GAMMA * b / ((0.001 + d2) * (a * d2.powf(b) + 1.0));
                points[j][0] += clip(coeff * dx) * alpha;
                points[j][1] += clip(coeff * dy) * alpha;
            }
            next_negative[i] += pushes as f64 * every_negative;
        }
        if (epoch + 1) % 10 == 0 || epoch + 1 == epochs {
            on_progress(Progress::Epoch {
                done: epoch + 1,
                total: epochs,
            });
        }
    }
}

/// SplitMix64: a fixed, portable stream, so a layout does not depend on
/// the platform's or a crate's idea of a default generator.
struct SplitMix(u64);

impl SplitMix {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        ((z as u128 * n as u128) >> 64) as usize
    }
}

/// The whole map: neighbours, a start seeded from `prior`, UMAP, and —
/// when there was a previous map — a fit back onto its frame, which
/// takes out whatever the run did to the map as a whole: a slow turn, a
/// drift, a swell.
///
/// A corpus of fewer than three documents has no neighbourhoods to
/// speak of and keeps its starting layout.
pub fn layout(
    corpus: &Corpus,
    prior: &HashMap<String, Point>,
    on_progress: &dyn Fn(Progress),
) -> Result<Layout, String> {
    let n = corpus.len();
    if n == 0 {
        return Ok(Layout {
            points: Vec::new(),
            seed: SeedStats::default(),
            epochs: 0,
        });
    }
    // UMAP counts the point itself among its neighbours, and wants
    // fewer neighbours than points.
    on_progress(Progress::Neighbours);
    let knn = nearest_neighbours(corpus, N_NEIGHBORS.min(n - 1));
    let (init, seed) = initial_layout(corpus, &knn, prior);
    if n < 3 {
        return Ok(Layout {
            points: init,
            seed,
            epochs: 0,
        });
    }
    let (epochs, rate) = if seed.seeded() {
        (SEEDED_EPOCHS, SEEDED_LEARNING_RATE)
    } else if n <= 10_000 {
        (FRESH_EPOCHS_SMALL, 1.0)
    } else {
        (FRESH_EPOCHS_LARGE, 1.0)
    };
    let mut points = umap(corpus, &knn, &init, epochs, rate, on_progress);
    if seed.seeded() {
        let (from, to): (Vec<Point>, Vec<Point>) = corpus
            .keys
            .iter()
            .zip(&points)
            .filter_map(|(key, p)| prior.get(key).map(|q| (*p, *q)))
            .unzip();
        if let Some(t) = fit_similarity(&from, &to) {
            for p in points.iter_mut() {
                *p = t.apply(*p);
            }
        }
    }
    if let Some(i) = points
        .iter()
        .position(|p| !p[0].is_finite() || !p[1].is_finite())
    {
        return Err(format!(
            "the layout put {:?} at a non-finite position",
            corpus.keys[i]
        ));
    }
    Ok(Layout {
        points,
        seed,
        epochs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three clusters on orthogonal axes, `per` points each, strung
    /// along an arc so that every point has neighbours of its own.
    fn clusters(per: usize, dim: usize) -> Corpus {
        let mut keys = Vec::new();
        let mut vectors = Vec::new();
        for c in 0..3 {
            for i in 0..per {
                let mut v = vec![0f32; dim];
                let t = 0.6 * i as f32 / per as f32;
                v[c] = t.cos();
                v[3 + c] = t.sin();
                unit(&mut v);
                vectors.extend(v);
                keys.push(format!("c{c}/{i}"));
            }
        }
        Corpus { keys, dim, vectors }
    }

    /// A curved sheet, `side × side` points, through `dim` dimensions:
    /// one connected piece, with two directions to spread along, as a
    /// real corpus has.
    fn sheet(side: usize, dim: usize) -> Corpus {
        let mut keys = Vec::new();
        let mut vectors = Vec::new();
        for i in 0..side {
            for j in 0..side {
                let (u, v) = (i as f32 / side as f32, j as f32 / side as f32);
                let mut x: Vec<f32> = (0..dim)
                    .map(|d| (u * (1 + d % 3) as f32 + v * (1 + d % 5) as f32 + d as f32).sin())
                    .collect();
                x[0] += 4.0;
                unit(&mut x);
                vectors.extend(x);
                keys.push(format!("{i},{j}"));
            }
        }
        Corpus { keys, dim, vectors }
    }

    fn dist(a: Point, b: Point) -> f32 {
        (a[0] - b[0]).hypot(a[1] - b[1])
    }

    fn centre(points: &[Point]) -> Point {
        let s = points
            .iter()
            .fold([0f32; 2], |s, p| [s[0] + p[0], s[1] + p[1]]);
        [s[0] / points.len() as f32, s[1] / points.len() as f32]
    }

    #[test]
    fn a_point_is_its_own_first_neighbour_even_beside_a_duplicate() {
        let corpus = Corpus {
            keys: vec!["a".into(), "a2".into(), "b".into()],
            dim: 2,
            vectors: vec![1.0, 0.0, 1.0, 0.0, 0.0, 1.0],
        };
        let knn = nearest_neighbours(&corpus, 2);
        assert_eq!(knn.neighbours(0), &[0, 1]);
        assert_eq!(knn.neighbours(1), &[1, 0]);
        assert_eq!(knn.neighbours(2)[0], 2);
        assert!(knn.dists.iter().all(|d| *d >= 0.0));
    }

    /// Neighbours are the nearest by cosine, in order, with distances
    /// `1 − cos`.
    #[test]
    fn neighbours_are_ranked_by_cosine_distance() {
        let mut vectors = Vec::new();
        for deg in [0f32, 10.0, 50.0, 90.0] {
            let r = deg.to_radians();
            vectors.extend([r.cos(), r.sin()]);
        }
        let corpus = Corpus {
            keys: (0..4).map(|i| i.to_string()).collect(),
            dim: 2,
            vectors,
        };
        let knn = nearest_neighbours(&corpus, 3);
        assert_eq!(knn.neighbours(0), &[0, 1, 2]);
        let want = 1.0 - 10f32.to_radians().cos();
        assert!((knn.dists[1] - want).abs() < 1e-6, "{}", knn.dists[1]);
    }

    #[test]
    fn the_same_similarity_is_recovered_with_and_without_a_flip() {
        let from: Vec<Point> = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 2.0], [3.0, 1.0]];
        for flip in [false, true] {
            let t = Similarity {
                from: [0.3, -0.2],
                to: [5.0, 7.0],
                scale: 2.5,
                cos: 0.6,
                sin: 0.8,
                flip,
            };
            let to: Vec<Point> = from.iter().map(|p| t.apply(*p)).collect();
            let got = fit_similarity(&from, &to).unwrap();
            for (p, q) in from.iter().zip(&to) {
                assert!(dist(got.apply(*p), *q) < 1e-4, "flip={flip}: {got:?}");
            }
            assert_eq!(got.flip, flip);
        }
    }

    #[test]
    fn a_first_run_starts_every_point_fresh() {
        let corpus = clusters(10, 8);
        let knn = nearest_neighbours(&corpus, 6);
        let (init, stats) = initial_layout(&corpus, &knn, &HashMap::new());
        assert_eq!(init.len(), 30);
        assert_eq!(
            stats,
            SeedStats {
                kept: 0,
                near_neighbours: 0,
                fresh: 30
            }
        );
    }

    /// A document the last map had starts where it was; a new one starts
    /// among the neighbours the last map placed.
    #[test]
    fn a_new_point_starts_among_its_mapped_neighbours() {
        let corpus = clusters(10, 8);
        let knn = nearest_neighbours(&corpus, 6);
        let mut prior = HashMap::new();
        for (i, key) in corpus.keys.iter().enumerate() {
            if key == "c1/3" {
                continue;
            }
            let c = i / 10;
            prior.insert(key.clone(), [100.0 * c as f32, 0.0]);
        }
        let (init, stats) = initial_layout(&corpus, &knn, &prior);
        assert_eq!(stats.kept, 29);
        assert_eq!(stats.near_neighbours, 1);
        assert_eq!(stats.fresh, 0);
        assert!(dist(init[13], [100.0, 0.0]) < 1.0, "{:?}", init[13]);
        assert!(dist(init[0], [0.0, 0.0]) < 1.0, "{:?}", init[0]);
    }

    /// The point of seeding: when documents arrive, the ones already on
    /// the map stay where they were — not rotated, flipped, rescaled or
    /// reshuffled. Points may still trade places locally, so what is
    /// measured is the typical point and each region's centre.
    ///
    /// The corpus is one connected sheet of a realistic size: UMAP holds
    /// nothing between disconnected pieces, so every run pushes those
    /// apart afresh, and a map a few `min_dist` across is all edge.
    #[test]
    fn documents_already_mapped_stay_put_when_more_arrive() {
        let full = sheet(40, 16);
        let arrives = |i: usize| i % 20 == 7;
        let before = Corpus {
            keys: (0..full.len())
                .filter(|&i| !arrives(i))
                .map(|i| full.keys[i].clone())
                .collect(),
            dim: full.dim,
            vectors: (0..full.len())
                .filter(|&i| !arrives(i))
                .flat_map(|i| full.row(i).to_vec())
                .collect(),
        };
        let first = layout(&before, &HashMap::new(), &|_| {}).unwrap();
        let prior: HashMap<String, Point> = before
            .keys
            .iter()
            .cloned()
            .zip(first.points.iter().copied())
            .collect();
        let second = layout(&full, &prior, &|_| {}).unwrap();
        assert_eq!(second.seed.kept, before.len());
        assert_eq!(second.seed.near_neighbours, full.len() - before.len());
        let (was, now): (Vec<Point>, Vec<Point>) = full
            .keys
            .iter()
            .zip(&second.points)
            .filter_map(|(k, p)| prior.get(k).map(|q| (*q, *p)))
            .unzip();
        let span = extent(&was);
        let mut moved: Vec<f32> = was.iter().zip(&now).map(|(a, b)| dist(*a, *b)).collect();
        moved.sort_by(f32::total_cmp);
        let median = moved[moved.len() / 2];
        assert!(median < 0.02 * span, "median moved {median} of {span}");
        for (a, b) in was.chunks(200).zip(now.chunks(200)) {
            let (a, b) = (centre(a), centre(b));
            assert!(dist(a, b) < 0.02 * span, "{a:?} → {b:?}");
        }
    }

    /// One thread and a fixed seed: the same start gives the same map.
    #[test]
    fn the_same_start_gives_the_same_map() {
        let corpus = clusters(20, 16);
        let a = layout(&corpus, &HashMap::new(), &|_| {}).unwrap();
        let b = layout(&corpus, &HashMap::new(), &|_| {}).unwrap();
        assert_eq!(a.points, b.points);
    }

    /// The clusters come out as clusters: every point is nearer its own
    /// cluster's centre than any other's.
    #[test]
    fn clusters_stay_apart() {
        let corpus = clusters(20, 16);
        let out = layout(&corpus, &HashMap::new(), &|_| {}).unwrap();
        let centres: Vec<Point> = (0..3)
            .map(|c| centre(&out.points[c * 20..(c + 1) * 20]))
            .collect();
        for (i, p) in out.points.iter().enumerate() {
            let own = i / 20;
            let nearest = (0..3)
                .min_by(|&a, &b| dist(*p, centres[a]).total_cmp(&dist(*p, centres[b])))
                .unwrap();
            assert_eq!(nearest, own, "point {i} at {p:?}");
        }
    }

    #[test]
    fn tiny_corpora_are_laid_out_without_umap() {
        for n in 0..3 {
            let corpus = Corpus {
                keys: (0..n).map(|i| i.to_string()).collect(),
                dim: 2,
                vectors: (0..n).flat_map(|i| [1.0, i as f32]).collect(),
            };
            let out = layout(&corpus, &HashMap::new(), &|_| {}).unwrap();
            assert_eq!(out.points.len(), n);
            assert_eq!(out.epochs, 0);
        }
    }

    #[test]
    fn a_corpus_smaller_than_the_neighbourhood_still_runs() {
        let corpus = clusters(2, 8);
        let out = layout(&corpus, &HashMap::new(), &|_| {}).unwrap();
        assert_eq!(out.points.len(), 6);
        assert!(out.epochs > 0);
    }
}
