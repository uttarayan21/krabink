//! Integer-only randomness for brushes. Every random value a stroke needs
//! is a pure function of the stroke's seed, the index of the dab that
//! wants it and which property it is for, so the same stroke lays the
//! same dabs on every platform, whether it is drawn live point by point
//! or all at once from the document. No state to carry, nothing to
//! resynchronise.

/// murmur3's 32-bit finaliser: a bijection that spreads every input bit
/// over every output bit.
fn fmix(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// A well-mixed word for (`seed`, `index`, `channel`).
pub(crate) fn hash(seed: u32, index: u32, channel: u32) -> u32 {
    let h = fmix(seed ^ 0x9e37_79b9);
    let h = fmix(h ^ index.wrapping_mul(0x85eb_ca6b));
    fmix(h ^ channel.wrapping_mul(0xc2b2_ae35))
}

/// Uniform in `0..1` (24 mantissa bits, so exact in `f32`).
pub(crate) fn unit(seed: u32, index: u32, channel: u32) -> f32 {
    (hash(seed, index, channel) >> 8) as f32 / (1u32 << 24) as f32
}

/// Uniform in `-1..=1`.
pub(crate) fn signed(seed: u32, index: u32, channel: u32) -> f32 {
    unit(seed, index, channel) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_in_range_and_depend_on_every_argument() {
        for i in 0..1000 {
            let u = unit(7, i, 0);
            assert!((0.0..1.0).contains(&u));
            let s = signed(7, i, 1);
            assert!((-1.0..=1.0).contains(&s));
        }
        assert_ne!(hash(1, 2, 3), hash(2, 2, 3));
        assert_ne!(hash(1, 2, 3), hash(1, 3, 3));
        assert_ne!(hash(1, 2, 3), hash(1, 2, 4));
        assert_eq!(hash(1, 2, 3), hash(1, 2, 3));
    }

    #[test]
    fn unit_is_roughly_uniform() {
        let n = 4096;
        let mean = (0..n).map(|i| unit(0, i, 0)).sum::<f32>() / n as f32;
        assert!((mean - 0.5).abs() < 0.03, "{mean}");
    }
}
