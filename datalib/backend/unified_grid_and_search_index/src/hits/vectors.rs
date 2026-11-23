//! What a search by meaning is asked with, and the arithmetic that scores
//! a chunk's vector against the query's.

use std::collections::HashSet;

/// What a search's structured terms keep of one document: what its
/// chunks may rank it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The document's own row: every chunk.
    Whole,
    /// Only these messages, by section id, and the text that is no
    /// message's (a page's body) when `own_text`, which is so when the
    /// document's own row is kept too: a chunk that covers none of them
    /// was written by someone, or at a time, the search leaves out.
    Sections {
        messages: HashSet<String>,
        own_text: bool,
    },
}

/// `(v·q, v·v)`, in one pass. On x86_64 the same loop is compiled a
/// second time for AVX2 and taken when the CPU has it; the baseline
/// target there has only SSE2. ARM's baseline NEON needs no second copy.
pub fn dot_and_norm(v: &[f32], q: &[f32]) -> (f32, f32) {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU was just found to have AVX2.
        return unsafe { dot_and_norm_avx2(v, q) };
    }
    dot_and_norm_portable(v, q)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn dot_and_norm_avx2(v: &[f32], q: &[f32]) -> (f32, f32) {
    dot_and_norm_portable(v, q)
}

/// Sixteen running sums of each, so the compiler can keep them in SIMD
/// registers: one sum would fix the order of every addition, and with
/// it a scalar loop (measured 6x slower on 768 dimensions).
#[inline(always)]
fn dot_and_norm_portable(v: &[f32], q: &[f32]) -> (f32, f32) {
    const LANES: usize = 16;
    let mut dot = [0f32; LANES];
    let mut norm = [0f32; LANES];
    let (vs, v_rest) = v.as_chunks::<LANES>();
    let (qs, q_rest) = q.as_chunks::<LANES>();
    for (a, b) in vs.iter().zip(qs) {
        for i in 0..LANES {
            dot[i] += a[i] * b[i];
            norm[i] += a[i] * a[i];
        }
    }
    let mut d: f32 = dot.iter().sum();
    let mut n: f32 = norm.iter().sum();
    for (a, b) in v_rest.iter().zip(q_rest) {
        d += a * b;
        n += a * a;
    }
    (d, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lengths on either side of the sixteen-wide body, the tail
    /// included, agree with the plain sums.
    #[test]
    fn dot_and_norm_agrees_with_the_plain_sums() {
        for len in [0, 1, 15, 16, 17, 768, 771] {
            let v: Vec<f32> = (0..len).map(|i| (i % 7) as f32 - 3.0).collect();
            let q: Vec<f32> = (0..len).map(|i| (i % 5) as f32 * 0.5).collect();
            let dot: f32 = v.iter().zip(&q).map(|(a, b)| a * b).sum();
            let norm: f32 = v.iter().map(|a| a * a).sum();
            assert_eq!(dot_and_norm(&v, &q), (dot, norm), "len {len}");
        }
    }
}
