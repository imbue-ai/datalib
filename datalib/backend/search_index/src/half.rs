//! Half precision (IEEE 754 binary16), as the bits of a `u16`: how search
//! holds a vector in memory, half the size of the f32 the cache stores.
//! Only finite values are read back: a held vector is unit length.

/// 2^112: rescales a binary16 exponent, shifted into an f32's place, from
/// binary16's bias (15) to f32's (127).
const REBIAS: f32 = f32::from_bits(0x7780_0000);

/// The value of finite binary16 bits, exactly. Branch-free, so a loop
/// over a vector of them compiles to vector instructions: the exponent
/// and mantissa shifted into an f32's place read as that value times
/// 2^-112, subnormals included, and one multiply undoes it.
#[inline]
pub fn to_f32(h: u16) -> f32 {
    let h = u32::from(h);
    let magnitude = f32::from_bits((h & 0x7fff) << 13) * REBIAS;
    f32::from_bits(magnitude.to_bits() | ((h & 0x8000) << 16))
}

/// The binary16 nearest `f`, ties to even; past binary16's range, infinity.
pub fn from_f32(f: f32) -> u16 {
    const INFINITY: u32 = 0xff << 23;
    const OVERFLOW: u32 = (127 + 16) << 23;
    const SUBNORMAL_BELOW: u32 = 113 << 23;
    const DENORMAL_MAGIC: u32 = ((127 - 15) + (23 - 10) + 1) << 23;
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let x = bits & 0x7fff_ffff;
    let magnitude = if x >= OVERFLOW {
        if x > INFINITY {
            0x7e00
        } else {
            0x7c00
        }
    } else if x < SUBNORMAL_BELOW {
        // Adding 0.5 x 2^-14 lines the binary16 subnormal's bits up with
        // the f32 mantissa's low bits, and the f32 add rounds them.
        let sum = f32::from_bits(x) + f32::from_bits(DENORMAL_MAGIC);
        (sum.to_bits() - DENORMAL_MAGIC) as u16
    } else {
        let odd = (x >> 13) & 1;
        let rebiased = x
            .wrapping_add(((15i32 - 127) << 23) as u32)
            .wrapping_add(0xfff + odd);
        (rebiased >> 13) as u16
    };
    magnitude | sign
}

/// `v · q`, with `v` in binary16.
pub fn dot(v: &[u16], q: &[f32]) -> f32 {
    const LANES: usize = 8;
    let mut acc = [0f32; LANES];
    let n = v.len().min(q.len());
    let (v_body, v_tail) = v[..n].as_chunks::<LANES>();
    let (q_body, q_tail) = q[..n].as_chunks::<LANES>();
    for (a, b) in v_body.iter().zip(q_body) {
        for i in 0..LANES {
            acc[i] += to_f32(a[i]) * b[i];
        }
    }
    let tail: f32 = v_tail.iter().zip(q_tail).map(|(a, b)| to_f32(*a) * b).sum();
    acc.iter().sum::<f32>() + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finite(h: u16) -> bool {
        h & 0x7c00 != 0x7c00
    }

    /// Every finite binary16 reads back as itself: `to_f32` is exact and
    /// `from_f32` returns it.
    #[test]
    fn every_finite_value_round_trips() {
        for h in (0..=u16::MAX).filter(|h| finite(*h)) {
            assert_eq!(from_f32(to_f32(h)), h, "{h:#06x}");
        }
        assert_eq!(to_f32(0x3c00), 1.0);
        assert_eq!(to_f32(0xc000), -2.0);
        assert_eq!(to_f32(0x0001), 2f32.powi(-24), "the smallest subnormal");
        assert_eq!(to_f32(0x7bff), 65504.0, "the largest finite");
    }

    /// An f32 goes to the nearest binary16, and a tie to the even one.
    #[test]
    fn an_f32_rounds_to_the_nearest_binary16() {
        let mut x = 1e-9f32;
        while x < 60000.0 {
            for f in [x, -x] {
                let h = from_f32(f);
                let err = (to_f32(h) - f).abs();
                for n in [h.wrapping_sub(1), h.wrapping_add(1)] {
                    if finite(n) && (n & 0x8000) == (h & 0x8000) {
                        let other = (to_f32(n) - f).abs();
                        assert!(
                            err < other || (err == other && h & 1 == 0),
                            "{f}: {h:#06x} is not the nearest"
                        );
                    }
                }
            }
            x *= 1.000_37;
        }
        let one_and_a_half_ulp = 1.0 + 3.0 * 2f32.powi(-11);
        assert_eq!(from_f32(1.0 + 2f32.powi(-11)), 0x3c00, "a tie goes to even");
        assert_eq!(from_f32(one_and_a_half_ulp), 0x3c02, "a tie goes to even");
        assert_eq!(from_f32(70000.0), 0x7c00, "past the range");
    }

    /// The dot of a binary16 unit vector with a query is the f32 dot to
    /// within what half precision keeps.
    #[test]
    fn a_dot_is_the_f32_dot_to_half_precision() {
        let dim = 771;
        let v: Vec<f32> = (0..dim).map(|i| (i * 37 % 101) as f32 - 50.0).collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let v: Vec<f32> = v.iter().map(|x| x / norm).collect();
        let q: Vec<f32> = (0..dim).map(|i| ((i * 13 % 7) as f32) - 3.0).collect();
        let exact: f32 = v.iter().zip(&q).map(|(a, b)| a * b).sum();
        let half: Vec<u16> = v.iter().map(|x| from_f32(*x)).collect();
        assert!((dot(&half, &q) - exact).abs() < 1e-2 * exact.abs().max(1.0));
    }
}
