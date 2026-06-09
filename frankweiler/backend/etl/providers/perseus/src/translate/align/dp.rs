//! Bertalign-style DP over very small sentence counts.
//!
//! Given pre-normalized sentence embeddings on both sides (shape
//! `[N, D]` each) plus character counts (for the length penalty),
//! returns a Pareto-optimal sequence of `(grc_indices, eng_indices)`
//! groups covering all sentences in order. Allowed transitions:
//!
//!   1:1, 1:2, 2:1, 1:3, 3:1
//!
//! This is enough for Smith's translation style (1 grc sentence
//! sometimes maps to 2-3 eng; the reverse is rare but supported).
//!
//! Cost per pair = `(1 - cos) + 0.1 * |ln((sum_grc_chars+1) /
//! (sum_eng_chars+1))|`. Matches the Python reference.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub grc: Vec<usize>,
    pub eng: Vec<usize>,
}

/// Normalize the (grc, eng) sentences in order into a sequence of
/// groupings. `grc_emb[i]` is the L2-normalized mean-pooled
/// embedding of the i-th Greek sentence. Same for eng.
pub fn align(
    grc_emb: &[Vec<f32>],
    eng_emb: &[Vec<f32>],
    grc_lens: &[usize],
    eng_lens: &[usize],
) -> Vec<Group> {
    let m = grc_emb.len();
    let n = eng_emb.len();
    assert_eq!(grc_lens.len(), m, "grc_lens size mismatch");
    assert_eq!(eng_lens.len(), n, "eng_lens size mismatch");

    if m == 0 || n == 0 {
        return Vec::new();
    }

    const INF: f64 = 1e18;
    // dp[i][j] = best cost aligning grc[0..i] with eng[0..j].
    let mut dp = vec![vec![INF; n + 1]; m + 1];
    // back[i][j] = (prev_i, prev_j) we transitioned from.
    let mut back: Vec<Vec<Option<(usize, usize)>>> = vec![vec![None; n + 1]; m + 1];
    dp[0][0] = 0.0;

    const MOVES: &[(usize, usize)] = &[(1, 1), (1, 2), (2, 1), (1, 3), (3, 1)];

    for i in 0..=m {
        for j in 0..=n {
            if dp[i][j] >= INF {
                continue;
            }
            for &(di, dj) in MOVES {
                let ni = i + di;
                let nj = j + dj;
                if ni > m || nj > n {
                    continue;
                }
                let cost = pair_cost(grc_emb, eng_emb, grc_lens, eng_lens, i, ni, j, nj);
                if dp[i][j] + cost < dp[ni][nj] {
                    dp[ni][nj] = dp[i][j] + cost;
                    back[ni][nj] = Some((i, j));
                }
            }
        }
    }

    if dp[m][n] >= INF {
        // Path couldn't reach (m,n) under our transition set (e.g.
        // very asymmetric counts like 1:5). Fall back to flat 1:1
        // pairing, leaving the longer side's tail bundled in.
        return fallback(m, n);
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut i = m;
    let mut j = n;
    while (i, j) != (0, 0) {
        let (pi, pj) = back[i][j].expect("path must exist when dp finite");
        groups.push(Group {
            grc: (pi..i).collect(),
            eng: (pj..j).collect(),
        });
        i = pi;
        j = pj;
    }
    groups.reverse();
    groups
}

fn pair_cost(
    grc_emb: &[Vec<f32>],
    eng_emb: &[Vec<f32>],
    grc_lens: &[usize],
    eng_lens: &[usize],
    gi: usize,
    gni: usize,
    ej: usize,
    enj: usize,
) -> f64 {
    let g = mean_normalize(grc_emb, gi, gni);
    let e = mean_normalize(eng_emb, ej, enj);
    let cos = dot_f32(&g, &e) as f64;
    let glen: usize = grc_lens[gi..gni].iter().sum();
    let elen: usize = eng_lens[ej..enj].iter().sum();
    let lp = ((glen as f64 + 1.0) / (elen as f64 + 1.0)).ln().abs() * 0.1;
    (1.0 - cos) + lp
}

fn mean_normalize(emb: &[Vec<f32>], start: usize, end: usize) -> Vec<f32> {
    let d = emb[start].len();
    let mut acc = vec![0.0f32; d];
    for v in &emb[start..end] {
        for k in 0..d {
            acc[k] += v[k];
        }
    }
    let n = (end - start) as f32;
    for x in acc.iter_mut() {
        *x /= n;
    }
    let norm: f32 = acc.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in acc.iter_mut() {
        *x /= norm;
    }
    acc
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn fallback(m: usize, n: usize) -> Vec<Group> {
    let k = m.min(n);
    let mut groups: Vec<Group> = (0..k)
        .map(|i| Group {
            grc: vec![i],
            eng: vec![i],
        })
        .collect();
    // Append unmatched tail on whichever side is longer.
    if m > k {
        if let Some(last) = groups.last_mut() {
            for i in k..m {
                last.grc.push(i);
            }
        }
    }
    if n > k {
        if let Some(last) = groups.last_mut() {
            for j in k..n {
                last.eng.push(j);
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_vec(d: usize, i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; d];
        v[i] = 1.0;
        v
    }

    #[test]
    fn aligns_one_to_one() {
        let g = vec![unit_vec(4, 0), unit_vec(4, 1)];
        let e = vec![unit_vec(4, 0), unit_vec(4, 1)];
        let groups = align(&g, &e, &[10, 10], &[10, 10]);
        assert_eq!(
            groups,
            vec![
                Group {
                    grc: vec![0],
                    eng: vec![0]
                },
                Group {
                    grc: vec![1],
                    eng: vec![1]
                },
            ]
        );
    }

    #[test]
    fn aligns_one_grc_to_two_eng() {
        // grc has one sentence; eng has two that are both close to it.
        let g = vec![unit_vec(4, 0)];
        let e = vec![unit_vec(4, 0), unit_vec(4, 0)];
        let groups = align(&g, &e, &[20], &[10, 10]);
        assert_eq!(
            groups,
            vec![Group {
                grc: vec![0],
                eng: vec![0, 1]
            }]
        );
    }

    #[test]
    fn aligns_two_grc_to_one_eng() {
        let g = vec![unit_vec(4, 0), unit_vec(4, 0)];
        let e = vec![unit_vec(4, 0)];
        let groups = align(&g, &e, &[10, 10], &[20]);
        assert_eq!(
            groups,
            vec![Group {
                grc: vec![0, 1],
                eng: vec![0]
            }]
        );
    }

    #[test]
    fn picks_one_to_two_over_misalignment() {
        // grc[0] aligns clearly with eng[0]; grc[1] aligns with the
        // concatenation of eng[1] and eng[2]. The misaligned 1:1
        // path would pair grc[1] with eng[1] only and leave eng[2]
        // dangling — invalid under our transition set, so DP must
        // pick the 1:2 transition for the second group.
        let g = vec![unit_vec(4, 0), unit_vec(4, 1)];
        let e = vec![unit_vec(4, 0), unit_vec(4, 1), unit_vec(4, 1)];
        let groups = align(&g, &e, &[10, 20], &[10, 10, 10]);
        assert_eq!(
            groups,
            vec![
                Group {
                    grc: vec![0],
                    eng: vec![0]
                },
                Group {
                    grc: vec![1],
                    eng: vec![1, 2]
                },
            ]
        );
    }

    #[test]
    fn empty_inputs_yield_empty_output() {
        assert!(align(&[], &[], &[], &[]).is_empty());
        let g = vec![unit_vec(4, 0)];
        assert!(align(&g, &[], &[10], &[]).is_empty());
    }

    #[test]
    fn five_to_one_falls_back_gracefully() {
        // 5:1 isn't in our transition set; fallback should still
        // produce a covering alignment without panicking.
        let g = vec![
            unit_vec(4, 0),
            unit_vec(4, 0),
            unit_vec(4, 0),
            unit_vec(4, 0),
            unit_vec(4, 0),
        ];
        let e = vec![unit_vec(4, 0)];
        let groups = align(&g, &e, &[1; 5], &[1]);
        // Every grc index appears exactly once; every eng index too.
        let mut g_seen: Vec<usize> = groups.iter().flat_map(|g| g.grc.clone()).collect();
        let mut e_seen: Vec<usize> = groups.iter().flat_map(|g| g.eng.clone()).collect();
        g_seen.sort();
        e_seen.sort();
        assert_eq!(g_seen, vec![0, 1, 2, 3, 4]);
        assert_eq!(e_seen, vec![0]);
    }
}
