//! Equalisation primitives: RBJ biquads, a 4th-order Linkwitz–Riley
//! high-pass, Regalia–Mitra allpass-based shelving/peaking sections, and a
//! 1/3-octave analysis bank.
//!
//! # Why Regalia–Mitra
//!
//! The [`RmHighShelf2`] and [`RmBell`] sections put their gain `K` in two
//! scalar mix coefficients and nowhere else:
//!
//! ```text
//! H(z) = (1 + K)/2  +  (1 − K)/2 · A(z)
//! ```
//!
//! where `A(z)` is an allpass whose coefficients depend only on frequency
//! and bandwidth. Two properties fall out, and dynamics processors want
//! both:
//!
//! 1. **`K` can be modulated per sample** with no coefficient recomputation
//!    — no `tan`/`cos` in the audio loop, no zipper, no block-boundary
//!    stair-stepping under a sub-millisecond attack.
//! 2. **At `K == 1` the section is `y = x` bit-exactly**, because the
//!    allpass term is multiplied by `(1 − 1)/2 == 0.0`. An idle dynamic EQ
//!    is a bare wire, not "almost" a wire.
//!
//! A conventional crossover-and-recombine split can offer neither: it
//! leaves its allpass phase rotation in the signal even at rest, and its
//! coefficients move with gain.
//!
//! Reference: P. A. Regalia and S. K. Mitra, "Tunable digital frequency
//! response equalization filters", IEEE Trans. ASSP, 1987.

use core::f32::consts::PI;

/// Flush denormals to zero. A compare+select, so no branch penalty, and it
/// keeps recursive states out of the denormal range where some hosts take a
/// large performance hit.
#[inline(always)]
fn fl(x: f32) -> f32 {
    if x.abs() < 1e-25 {
        0.0
    } else {
        x
    }
}

/// Butterworth `Q` — the flat-magnitude value for a single 2nd-order
/// section, and the value both halves of a Linkwitz–Riley 4th-order pair
/// use.
pub const BUTTERWORTH_Q: f32 = core::f32::consts::FRAC_1_SQRT_2;

// ---------------------------------------------------------------------------
// Biquad
// ---------------------------------------------------------------------------

/// Normalised biquad coefficients (`a0` divided out). Separate from
/// [`BiquadState`] so a stereo pair shares one set of coefficients.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::identity()
    }
}

impl Biquad {
    /// Pass-through.
    pub const fn identity() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }

    /// RBJ low-pass. `f0` is clamped to a stable fraction of `sr`.
    pub fn lowpass(f0: f32, q: f32, sr: f32) -> Self {
        let (cw, alpha) = Self::prewarp(f0, q, sr);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - cw) * 0.5;
        Self {
            b0: b0 / a0,
            b1: (1.0 - cw) / a0,
            b2: b0 / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    /// RBJ high-pass.
    pub fn highpass(f0: f32, q: f32, sr: f32) -> Self {
        let (cw, alpha) = Self::prewarp(f0, q, sr);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 + cw) * 0.5;
        Self {
            b0: b0 / a0,
            b1: -(1.0 + cw) / a0,
            b2: b0 / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    /// RBJ constant-0 dB-peak band-pass — unity gain at `f0`.
    pub fn bandpass(f0: f32, q: f32, sr: f32) -> Self {
        let (cw, alpha) = Self::prewarp(f0, q, sr);
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    #[inline]
    fn prewarp(f0: f32, q: f32, sr: f32) -> (f32, f32) {
        let nyq = sr * 0.5;
        let f0 = f0.clamp(1.0, nyq * 0.98);
        let q = q.max(1e-3);
        let w0 = 2.0 * PI * f0 / sr;
        let (sw, cw) = (w0.sin(), w0.cos());
        (cw, sw / (2.0 * q))
    }
}

/// Per-channel state for a [`Biquad`], transposed direct form II (two
/// state words, good float behaviour).
#[derive(Clone, Copy, Debug, Default)]
pub struct BiquadState {
    z1: f32,
    z2: f32,
}

impl BiquadState {
    #[inline]
    pub fn tick(&mut self, c: &Biquad, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = fl(c.b1 * x - c.a1 * y + self.z2);
        self.z2 = fl(c.b2 * x - c.a2 * y);
        y
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// Linkwitz–Riley 4th-order high-pass
// ---------------------------------------------------------------------------

/// 4th-order Linkwitz–Riley high-pass: two cascaded Butterworth sections,
/// 24 dB/oct, −6 dB at the corner, no resonant peak.
///
/// This is the de-esser's sidechain filter. A 12 dB/oct high-pass at
/// 6.5 kHz is still only −12 dB an octave down, which is exactly where a
/// bright vowel's third formant lives — that leakage is what makes cheap
/// de-essers duck on sustained "aah". At 24 dB/oct the vowel stops voting.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lr4Hp {
    coeffs: Biquad,
    s1: BiquadState,
    s2: BiquadState,
}

impl Lr4Hp {
    pub fn new(f0: f32, sr: f32) -> Self {
        Self { coeffs: Biquad::highpass(f0, BUTTERWORTH_Q, sr), ..Default::default() }
    }

    /// Retune. Leaves the state alone so a moving control doesn't click.
    pub fn set_freq(&mut self, f0: f32, sr: f32) {
        self.coeffs = Biquad::highpass(f0, BUTTERWORTH_Q, sr);
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.s1.tick(&self.coeffs, x);
        self.s2.tick(&self.coeffs, y)
    }

    pub fn reset(&mut self) {
        self.s1.reset();
        self.s2.reset();
    }
}

// ---------------------------------------------------------------------------
// Regalia–Mitra sections
// ---------------------------------------------------------------------------

/// First-order allpass, `A(z) = (a + z⁻¹) / (1 + a·z⁻¹)`, with its 90°
/// phase point at `f0`. `+1` at DC, `−1` at Nyquist.
#[derive(Clone, Copy, Debug, Default)]
pub struct Allpass1 {
    a: f32,
    x1: f32,
    y1: f32,
}

impl Allpass1 {
    pub fn new(f0: f32, sr: f32) -> Self {
        let mut s = Self::default();
        s.set_freq(f0, sr);
        s
    }

    pub fn set_freq(&mut self, f0: f32, sr: f32) {
        let nyq = sr * 0.5;
        let f0 = f0.clamp(1.0, nyq * 0.98);
        let t = (PI * f0 / sr).tan();
        self.a = (t - 1.0) / (t + 1.0);
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.a * x + self.x1 - self.a * self.y1;
        self.x1 = fl(x);
        self.y1 = fl(y);
        y
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

/// Two cascaded Regalia–Mitra first-order high shelves — unity below `f0`,
/// gain `k_half²` above.
///
/// This is *numerically identical* to a 2nd-order Linkwitz–Riley crossover
/// split `LP² − K·HP²` at every frequency (see the module tests), i.e. it
/// is exactly the classic 12 dB/oct phase-coherent split-band de-esser
/// response — but transparent at rest and modulatable per sample.
///
/// Note the consequence, which is a property of *any* such split and not of
/// this implementation: at `f0` itself the response sits at **half** the
/// total depth in dB, reaching full depth roughly an octave above.
#[derive(Clone, Copy, Debug, Default)]
pub struct RmHighShelf2 {
    s1: Allpass1,
    s2: Allpass1,
}

impl RmHighShelf2 {
    pub fn new(f0: f32, sr: f32) -> Self {
        Self { s1: Allpass1::new(f0, sr), s2: Allpass1::new(f0, sr) }
    }

    pub fn set_freq(&mut self, f0: f32, sr: f32) {
        self.s1.set_freq(f0, sr);
        self.s2.set_freq(f0, sr);
    }

    /// `k_half` is the **per-stage** gain; the shelf's total high-frequency
    /// gain is `k_half²`. Callers working in dB should pass
    /// `db_to_gain(total_db * 0.5)`, which keeps `total_db == 0.0`
    /// exactly transparent without a `sqrt`.
    #[inline]
    pub fn tick(&mut self, x: f32, k_half: f32) -> f32 {
        let m0 = 0.5 * (1.0 + k_half);
        let m1 = 0.5 * (1.0 - k_half);
        let y1 = m0 * x + m1 * self.s1.tick(x);
        m0 * y1 + m1 * self.s2.tick(y1)
    }

    pub fn reset(&mut self) {
        self.s1.reset();
        self.s2.reset();
    }
}

/// Regalia–Mitra peaking (bell) section — unity away from `f0`, gain `k`
/// at `f0`.
///
/// The bandwidth coefficient is deliberately **gain-independent**, which
/// makes cuts narrower than boosts of the same magnitude. For a section
/// that only ever cuts that reads as proportional-Q: gentle reduction stays
/// broad, deep reduction turns surgical. Restoring textbook symmetry would
/// put `k` back into the coefficients and cost the per-sample modulation.
#[derive(Clone, Copy, Debug, Default)]
pub struct RmBell {
    c: f32,
    d: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl RmBell {
    pub fn new(f0: f32, q: f32, sr: f32) -> Self {
        let mut s = Self::default();
        s.set_freq(f0, q, sr);
        s
    }

    pub fn set_freq(&mut self, f0: f32, q: f32, sr: f32) {
        let nyq = sr * 0.5;
        let f0 = f0.clamp(1.0, nyq * 0.98);
        let q = q.clamp(0.1, 40.0);
        let w0 = 2.0 * PI * f0 / sr;
        // Bandwidth in rad/sample, clamped so tan() stays finite.
        let wb = (w0 / q).clamp(1e-4, PI * 0.98);
        let tb = (wb * 0.5).tan();
        self.c = (tb - 1.0) / (tb + 1.0);
        self.d = -w0.cos();
    }

    #[inline]
    pub fn tick(&mut self, x: f32, k: f32) -> f32 {
        let e = self.d * (1.0 - self.c);
        // Second-order allpass: +1 at DC and Nyquist, −1 at f0.
        let ap = -self.c * x + e * self.x1 + self.x2 - e * self.y1 + self.c * self.y2;
        self.x2 = self.x1;
        self.x1 = fl(x);
        self.y2 = self.y1;
        self.y1 = fl(ap);
        0.5 * (1.0 + k) * x + 0.5 * (1.0 - k) * ap
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

// ---------------------------------------------------------------------------
// 1/3-octave analysis bank
// ---------------------------------------------------------------------------

/// Number of bands in [`ThirdOctaveBank`] — ISO centres 2 k … 16 kHz.
pub const THIRD_OCTAVE_BANDS: usize = 10;

/// ISO 1/3-octave centre frequencies covering the sibilance range.
pub const THIRD_OCTAVE_CENTRES: [f32; THIRD_OCTAVE_BANDS] =
    [2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0];

/// Parallel 1/3-octave band-pass bank with per-band peak tracking — the
/// analysis side of a sibilance display. Cheap enough to leave running:
/// ten biquads plus ten `max` operations per sample.
#[derive(Clone, Copy, Debug)]
pub struct ThirdOctaveBank {
    coeffs: [Biquad; THIRD_OCTAVE_BANDS],
    state: [BiquadState; THIRD_OCTAVE_BANDS],
    peak: [f32; THIRD_OCTAVE_BANDS],
}

impl ThirdOctaveBank {
    /// `Q` for 1/3-octave bands is `2^(1/6) / (2^(1/3) − 1) ≈ 4.318`.
    pub const Q: f32 = 4.318;

    pub fn new(sr: f32) -> Self {
        let mut coeffs = [Biquad::identity(); THIRD_OCTAVE_BANDS];
        for (i, c) in coeffs.iter_mut().enumerate() {
            *c = Biquad::bandpass(THIRD_OCTAVE_CENTRES[i], Self::Q, sr);
        }
        Self {
            coeffs,
            state: [BiquadState::default(); THIRD_OCTAVE_BANDS],
            peak: [0.0; THIRD_OCTAVE_BANDS],
        }
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        for (i, c) in self.coeffs.iter_mut().enumerate() {
            *c = Biquad::bandpass(THIRD_OCTAVE_CENTRES[i], Self::Q, sr);
        }
        self.reset();
    }

    /// Run one sample through every band, accumulating per-band peaks.
    #[inline]
    pub fn tick(&mut self, x: f32) {
        for i in 0..THIRD_OCTAVE_BANDS {
            let y = self.state[i].tick(&self.coeffs[i], x).abs();
            if y > self.peak[i] {
                self.peak[i] = y;
            }
        }
    }

    /// Peaks accumulated since the last [`Self::take_peaks`].
    pub fn take_peaks(&mut self) -> [f32; THIRD_OCTAVE_BANDS] {
        let out = self.peak;
        self.peak = [0.0; THIRD_OCTAVE_BANDS];
        out
    }

    pub fn reset(&mut self) {
        self.state = [BiquadState::default(); THIRD_OCTAVE_BANDS];
        self.peak = [0.0; THIRD_OCTAVE_BANDS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Steady-state magnitude response, measured by actually running a sine
    /// through the difference equations rather than re-evaluating a formula.
    fn magnitude(f: f32, mut tick: impl FnMut(f32) -> f32) -> f32 {
        let n = 24_000;
        let settle = n / 2;
        let mut acc_in = 0.0f64;
        let mut acc_out = 0.0f64;
        for i in 0..n {
            let x = (2.0 * PI * f * i as f32 / SR).sin();
            let y = tick(x);
            if i >= settle {
                acc_in += (x as f64) * (x as f64);
                acc_out += (y as f64) * (y as f64);
            }
        }
        (acc_out / acc_in).sqrt() as f32
    }

    fn db(x: f32) -> f32 {
        20.0 * x.log10()
    }

    #[test]
    fn butterworth_lowpass_is_minus_three_db_at_corner() {
        let c = Biquad::lowpass(1000.0, BUTTERWORTH_Q, SR);
        let mut s = BiquadState::default();
        let m = magnitude(1000.0, |x| s.tick(&c, x));
        assert!((db(m) + 3.0).abs() < 0.3, "got {} dB", db(m));
    }

    #[test]
    fn lr4_highpass_is_minus_six_db_at_corner_and_24_db_per_octave() {
        let mut f = Lr4Hp::new(6500.0, SR);
        let at_fc = db(magnitude(6500.0, |x| f.tick(x)));
        assert!((at_fc + 6.0).abs() < 0.4, "at fc: {at_fc} dB");

        f.reset();
        let one_oct_down = db(magnitude(3250.0, |x| f.tick(x)));
        f.reset();
        let two_oct_down = db(magnitude(1625.0, |x| f.tick(x)));
        // Asymptotic slope: another ~24 dB per octave below the corner.
        let slope = one_oct_down - two_oct_down;
        assert!((slope - 24.0).abs() < 2.0, "slope {slope} dB/oct");
        assert!(one_oct_down < -20.0, "one octave down only {one_oct_down} dB");
    }

    #[test]
    fn rm_shelf_is_unity_at_dc_and_k_squared_at_top() {
        // Total depth −8 dB → per-stage gain 10^(−4/20).
        let k_half = 10f32.powf(-4.0 / 20.0);
        let mut s = RmHighShelf2::new(6500.0, SR);
        let low = db(magnitude(100.0, |x| s.tick(x, k_half)));
        assert!(low.abs() < 0.2, "LF should be unity, got {low} dB");

        s.reset();
        let top = db(magnitude(22_000.0, |x| s.tick(x, k_half)));
        assert!((top + 8.0).abs() < 0.5, "HF should be −8 dB, got {top} dB");

        s.reset();
        let at_fc = db(magnitude(6500.0, |x| s.tick(x, k_half)));
        // Half the total depth at the corner — a property of the split.
        assert!((at_fc + 3.11).abs() < 0.3, "at fc: {at_fc} dB");
    }

    /// The guarantee the whole design rests on: no reduction means the
    /// section is a wire, sample for sample, not merely close to one.
    #[test]
    fn rm_sections_are_bit_exact_at_unity_gain() {
        let mut shelf = RmHighShelf2::new(6500.0, SR);
        let mut bell = RmBell::new(8000.0, 2.0, SR);
        for i in 0..2000 {
            let x = (i as f32 * 0.037).sin() * 0.6 + (i as f32 * 0.31).sin() * 0.3;
            assert_eq!(shelf.tick(x, 1.0), x, "shelf diverged at {i}");
            assert_eq!(bell.tick(x, 1.0), x, "bell diverged at {i}");
        }
    }

    #[test]
    fn rm_bell_cuts_only_at_centre() {
        let k = 10f32.powf(-8.0 / 20.0);
        let mut b = RmBell::new(8000.0, 2.0, SR);
        let centre = db(magnitude(8000.0, |x| b.tick(x, k)));
        assert!((centre + 8.0).abs() < 0.3, "centre: {centre} dB");

        b.reset();
        let below = db(magnitude(1000.0, |x| b.tick(x, k)));
        assert!(below.abs() < 0.3, "3 octaves below: {below} dB");

        b.reset();
        let above = db(magnitude(20_000.0, |x| b.tick(x, k)));
        assert!(above.abs() < 0.5, "well above: {above} dB");
    }

    /// Proportional-Q by construction: the notch narrows as it deepens.
    #[test]
    fn rm_bell_narrows_as_it_deepens() {
        fn half_depth_width(depth_db: f32) -> f32 {
            let k = 10f32.powf(-depth_db / 20.0);
            let half = -depth_db * 0.5;
            let (mut lo, mut hi) = (0.0f32, 0.0f32);
            let mut f = 2000.0f32;
            while f < 22_000.0 {
                let mut b = RmBell::new(8000.0, 2.0, SR);
                let d = db(magnitude(f, |x| b.tick(x, k)));
                if d < half {
                    if lo == 0.0 {
                        lo = f;
                    }
                    hi = f;
                }
                f *= 1.03;
            }
            hi - lo
        }
        let shallow = half_depth_width(3.0);
        let deep = half_depth_width(15.0);
        assert!(deep < shallow * 0.75, "shallow {shallow} Hz, deep {deep} Hz");
    }

    /// The two-stage Regalia–Mitra shelf reproduces a 2nd-order
    /// Linkwitz–Riley crossover split `LP² − K·HP²` exactly. Same response,
    /// but transparent at rest and modulatable per sample — which is the
    /// entire argument for using it instead of an actual crossover.
    #[test]
    fn rm_shelf_equals_linkwitz_riley_split() {
        const F0: f32 = 6500.0;
        let k = 10f32.powf(-8.0 / 20.0);
        let k_half = 10f32.powf(-4.0 / 20.0);

        // LR2 legs: two cascaded one-pole sections each.
        struct OnePole {
            b: f32,
            a: f32,
            x1: f32,
            y1: f32,
            hp: bool,
        }
        impl OnePole {
            fn new(f0: f32, sr: f32, hp: bool) -> Self {
                let t = (PI * f0 / sr).tan();
                let (b, a) = if hp { (1.0 / (1.0 + t), (t - 1.0) / (1.0 + t)) } else { (t / (1.0 + t), (t - 1.0) / (1.0 + t)) };
                Self { b, a, x1: 0.0, y1: 0.0, hp }
            }
            fn tick(&mut self, x: f32) -> f32 {
                let sign = if self.hp { -1.0 } else { 1.0 };
                let y = self.b * x + sign * self.b * self.x1 - self.a * self.y1;
                self.x1 = x;
                self.y1 = y;
                y
            }
        }

        for &f in &[1000.0f32, 3250.0, 4600.0, 6500.0, 9200.0, 13000.0, 20000.0] {
            let mut shelf = RmHighShelf2::new(F0, SR);
            let a = db(magnitude(f, |x| shelf.tick(x, k_half)));

            let mut lp1 = OnePole::new(F0, SR, false);
            let mut lp2 = OnePole::new(F0, SR, false);
            let mut hp1 = OnePole::new(F0, SR, true);
            let mut hp2 = OnePole::new(F0, SR, true);
            let b = db(magnitude(f, |x| {
                let lo = lp2.tick(lp1.tick(x));
                let hi = hp2.tick(hp1.tick(x));
                lo - k * hi
            }));

            assert!((a - b).abs() < 0.25, "{f} Hz: shelf {a} dB vs LR2 split {b} dB");
        }
    }

    #[test]
    fn third_octave_bank_localises_a_sine() {
        let mut bank = ThirdOctaveBank::new(SR);
        // 8 kHz is band index 6.
        for i in 0..8000 {
            bank.tick((2.0 * PI * 8000.0 * i as f32 / SR).sin());
            if i == 4000 {
                // Discard the filters' startup transient; in real use the
                // bank is read at ~30 Hz, so a one-time onset splash never
                // dominates a report.
                bank.take_peaks();
            }
        }
        let peaks = bank.take_peaks();
        let hottest = peaks
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(hottest, 6, "peaks: {peaks:?}");
        assert!(peaks[6] > peaks[0] * 10.0, "poor rejection: {peaks:?}");
        // take_peaks clears.
        assert_eq!(bank.take_peaks(), [0.0; THIRD_OCTAVE_BANDS]);
    }
}
