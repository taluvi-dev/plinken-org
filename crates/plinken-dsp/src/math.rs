//! Small math helpers shared across the DSP primitives.

/// Convert MIDI note to frequency
#[inline]
pub fn midi_to_freq(note: f32) -> f32 {
    440.0 * (2.0f32).powf((note - 69.0) / 12.0)
}

/// Fast tanh approximation for saturation
#[inline]
pub fn fast_tanh(x: f32) -> f32 {
    let x2 = x * x;
    x * (27.0 + x2) / (27.0 + 9.0 * x2)
}

/// Soft clipping for gentle saturation
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    if x > 1.0 {
        1.0 - 1.0 / (1.0 + (x - 1.0) * 2.0)
    } else if x < -1.0 {
        -1.0 + 1.0 / (1.0 + (-x - 1.0) * 2.0)
    } else {
        x
    }
}

/// `10·log10(x) = LOG2_TO_10LOG10 · log2(x)` — mean-square to dB.
pub const LOG2_TO_10LOG10: f32 = 3.010_3;

/// `10^(db/20) = 2^(db · DB_TO_EXP2)` — dB to linear gain.
pub const DB_TO_EXP2: f32 = 0.166_096;

/// Fast `log2(x)` — exponent straight off the float bits, minimax
/// quadratic for the mantissa. Max absolute error ≈ 0.01 (≈ 0.03 dB as a
/// power ratio), which is far below audibility for a detector. Returns
/// `-126.0` for zero and negatives so a silent mean-square lands at a
/// finite floor instead of `-inf`.
#[inline]
pub fn fast_log2(x: f32) -> f32 {
    if x <= 0.0 {
        return -126.0;
    }
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    // Mantissa remapped to [1, 2).
    let m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    // Minimax quadratic for log2(m) on [1, 2).
    let p = (-0.344_845 * m + 2.024_658) * m - 1.674_873;
    exp as f32 + p
}

/// Fast `2^x` — integer part into the float exponent, cubic minimax for
/// the fraction. Max relative error ≈ 2e-4 (≈ 0.002 dB as a gain), and
/// **exact at integer inputs**, so `fast_exp2(0) == 1.0` and a settled
/// 0 dB gain stage is bit-transparent rather than merely close. Input is
/// clamped to ±120 (≈ ±720 dB) to stay inside the f32 exponent range.
#[inline]
pub fn fast_exp2(x: f32) -> f32 {
    let x = x.clamp(-120.0, 120.0);
    let xi = x.floor();
    let xf = x - xi;
    // Cubic minimax for 2^f on [0, 1).
    let p = 1.0 + xf * (0.695_837 + xf * (0.224_824 + xf * 0.079_339));
    let scale = f32::from_bits(((xi as i32 + 127) as u32) << 23);
    scale * p
}

/// Linear gain for `db`, via [`fast_exp2`]. Exactly `1.0` at `db == 0.0`.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    fast_exp2(db * DB_TO_EXP2)
}

/// dB for a mean-square (power) value, via [`fast_log2`]. Floors at
/// roughly −379 dB rather than diverging on silence.
#[inline]
pub fn ms_to_db(ms: f32) -> f32 {
    LOG2_TO_10LOG10 * fast_log2(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_to_freq_reference_pitches() {
        assert!((midi_to_freq(69.0) - 440.0).abs() < 1e-3);
        assert!((midi_to_freq(57.0) - 220.0).abs() < 1e-3);
        assert!((midi_to_freq(60.0) - 261.6256).abs() < 1e-2);
    }

    #[test]
    fn fast_tanh_tracks_tanh() {
        for i in -20..=20 {
            let x = i as f32 * 0.1;
            assert!((fast_tanh(x) - x.tanh()).abs() < 0.03, "x={x}");
        }
    }

    #[test]
    fn soft_clip_bounded_and_passthrough() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.5), -0.5);
        assert!(soft_clip(10.0) <= 1.5);
        assert!(soft_clip(-10.0) >= -1.5);
        assert!(soft_clip(10.0) > soft_clip(2.0));
    }

    #[test]
    fn fast_log2_tracks_log2() {
        for i in 1..2000 {
            let x = i as f32 * 0.01; // 0.01 .. 20
            assert!((fast_log2(x) - x.log2()).abs() < 0.01, "x={x}");
        }
        // Tiny mean-square values (deep silence) still behave.
        assert!((fast_log2(1e-12) - (1e-12f32).log2()).abs() < 0.01);
        assert_eq!(fast_log2(0.0), -126.0);
        assert_eq!(fast_log2(-1.0), -126.0);
    }

    #[test]
    fn fast_exp2_tracks_exp2() {
        for i in -400..=400 {
            let x = i as f32 * 0.05; // -20 .. 20
            let rel = (fast_exp2(x) - x.exp2()).abs() / x.exp2();
            assert!(rel < 3e-4, "x={x}");
        }
        assert_eq!(fast_exp2(0.0), 1.0);
        assert_eq!(fast_exp2(1.0), 2.0);
    }

    /// The whole "an idle de-esser is a wire" guarantee rests on this:
    /// zero dB must map to *exactly* unity, not 0.9999.
    #[test]
    fn db_to_gain_is_exactly_unity_at_zero() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-6.0) - 0.501_187).abs() < 1e-3);
        assert!((db_to_gain(-20.0) - 0.1).abs() < 1e-3);
    }

    #[test]
    fn ms_to_db_matches_ten_log_ten() {
        for &ms in &[1.0f32, 0.5, 0.1, 1e-3, 1e-6] {
            let want = 10.0 * ms.log10();
            assert!((ms_to_db(ms) - want).abs() < 0.05, "ms={ms}");
        }
    }
}
