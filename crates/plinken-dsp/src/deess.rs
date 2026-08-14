//! Esscape de-esser core — program-adaptive sibilance reduction.
//!
//! Full design rationale in `plugins/com.plinken/esscape/DESIGN.md`. The
//! shape in one paragraph: a dbx 902-style **relative detector** compares
//! the level of a user-tuned HF band against a 250 Hz-high-passed
//! reference, both through matched 2 ms RMS averagers, entirely in dB. The
//! resulting "tilt" barely moves when the singer moves — vowels sit around
//! −25…−35 dB, esses at −6…−1 dB — so one threshold works across a huge
//! dynamic range. Reduction is applied through Regalia–Mitra sections
//! ([`crate::eq`]) whose gain is a per-sample scalar: the idle plugin is a
//! bit-exact wire.
//!
//! Contract mirrors the other cores in this crate: `set_params` once per
//! block, allocation only in `new` / `set_sample_rate`, f32 everywhere,
//! wasm32-clean, no dependencies.

use crate::eq::{Biquad, BiquadState, Lr4Hp, RmBell, RmHighShelf2, BUTTERWORTH_Q};
use crate::math::{db_to_gain, ms_to_db};

/// Reference-band high-pass corner: "the voice band". Keeps bass bleed on a
/// bus from inflating the reference and stalling the detector.
const REF_HP_HZ: f32 = 250.0;

/// RMS averager time constant, seconds — matched between the HF and
/// reference detectors (mismatch turns every transient into a false tilt
/// spike). Also the source of the program-dependent attack: a fixed-τ
/// mean-square climbs faster in dB for a bigger step, reproducing the 902's
/// 2 ms / 600 µs published behaviour without a fitted curve.
const RMS_TAU: f32 = 0.002;

/// Detector gate: below this reference level (dBFS) the target reduction
/// is forced to zero. Without it the relative detector happily de-esses
/// room tone, whose tilt is high and whose level is irrelevant.
const GATE_DB: f32 = -70.0;

/// Post-ballistics gain smoother t63, seconds. Rounds the release-slew
/// corner and bandlimits the gain signal (≈ 800 Hz), which is what keeps
/// modulation sidebands under Nyquist without oversampling.
const SMOOTH_SEC: f32 = 0.0002;

/// Lookahead depth when enabled, seconds. §6.1 of the design: sibilants
/// rise over tens of milliseconds, so 2 ms covers the only genuinely spiky
/// case (affricate stop bursts) without audibly pre-ducking vowel tails.
pub const LOOKAHEAD_SEC: f32 = 0.002;

/// Reduction shaper topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// 12 dB/oct high-shelf cut above `freq_hz` — identical to the classic
    /// LR2 phase-coherent split (the 902's HF-ONLY mode).
    Shelf,
    /// Proportional-Q notch centred between `freq_hz` and the LP corner —
    /// the modern "dynamic notch" sound; leaves the air band intact.
    Bell,
}

/// Monitor tap — what the plugin's output carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Monitor {
    /// Normal processing.
    Off,
    /// The sidechain band itself — sweep `freq_hz` until the ess is
    /// loudest. Detection and metering keep running.
    Listen,
    /// `delayed input − output`: exactly what is being removed. Hearing
    /// vowel in the delta means the detector is over-reaching.
    Delta,
}

/// Detector threshold interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectMode {
    /// dbx 902 style: threshold is relative to the reference band's level
    /// (spectral tilt). Level-independent — the default.
    Relative,
    /// Threshold is plain dBFS on the sidechain band (Pro-DS "Allround").
    Absolute,
}

/// Block-rate parameters. Plain data; clamp/scale at the plugin boundary.
#[derive(Clone, Copy, Debug)]
pub struct DeEssParams {
    /// Sidechain high-pass corner and shaper frequency, Hz (2 k – 16 k).
    pub freq_hz: f32,
    /// Sidechain low-pass corner, Hz. At or above 19 999 the low-pass is
    /// bypassed ("off").
    pub lp_hz: f32,
    /// Threshold, dB. Read as tilt-dB in [`DetectMode::Relative`], dBFS in
    /// [`DetectMode::Absolute`]. −45 … 0.
    pub threshold_db: f32,
    /// Maximum reduction, dB (0 – 20). The 902's RANGE.
    pub range_db: f32,
    /// Static-curve character, 0 – 1: knee 12→0 dB, slope 0.5→0.95.
    pub detection: f32,
    /// Attack, ms (0.2 – 5).
    pub attack_ms: f32,
    /// Release, ms (10 – 250), specified as "time to recover 10 dB".
    pub release_ms: f32,
    /// Wide↔Split blend, 0 – 1. 0 = broadband gain only, 1 = shaper only.
    pub mode: f32,
    pub shape: Shape,
    pub detect: DetectMode,
    pub monitor: Monitor,
    /// Stereo link, 0 – 1. 1 = both channels take the louder detector.
    pub link: f32,
    /// Enable the 2 ms lookahead delay. The *host-reported* latency must
    /// follow this — see [`DeEsser::latency_samples`].
    pub lookahead: bool,
}

impl Default for DeEssParams {
    fn default() -> Self {
        Self {
            freq_hz: 6500.0,
            lp_hz: 20_000.0,
            threshold_db: -14.0,
            range_db: 8.0,
            detection: 0.5,
            attack_ms: 0.8,
            release_ms: 70.0,
            mode: 1.0,
            shape: Shape::Shelf,
            detect: DetectMode::Relative,
            monitor: Monitor::Off,
            link: 1.0,
            lookahead: false,
        }
    }
}

/// One-pole mean-square RMS detector reporting in dB.
#[derive(Clone, Copy, Debug, Default)]
struct RmsDb {
    ms: f32,
    coeff: f32,
}

impl RmsDb {
    fn set_sample_rate(&mut self, sr: f32) {
        self.coeff = 1.0 - (-1.0 / (RMS_TAU * sr)).exp();
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        self.ms += (x * x - self.ms) * self.coeff;
        // Mean-square → dB is 10·log10, i.e. the RMS level in 20·log10 terms.
        ms_to_db(self.ms + 1e-30)
    }

    fn reset(&mut self) {
        self.ms = 0.0;
    }
}

/// Per-channel detector chain: sidechain band + reference band.
#[derive(Clone, Copy, Debug, Default)]
struct Detector {
    sc_hp: Lr4Hp,
    sc_lp_coeffs: Biquad,
    sc_lp: BiquadState,
    lp_active: bool,
    ref_coeffs: Biquad,
    ref_hp: BiquadState,
    rms_hf: RmsDb,
    rms_ref: RmsDb,
}

impl Detector {
    fn configure(&mut self, freq_hz: f32, lp_hz: f32, sr: f32) {
        self.sc_hp.set_freq(freq_hz, sr);
        self.lp_active = lp_hz < 19_999.0;
        if self.lp_active {
            // Keep the LP meaningfully above the HP so the band never
            // pinches shut.
            let lp = lp_hz.max(freq_hz * 1.25);
            self.sc_lp_coeffs = Biquad::lowpass(lp, BUTTERWORTH_Q, sr);
        }
        self.ref_coeffs = Biquad::highpass(REF_HP_HZ, BUTTERWORTH_Q, sr);
    }

    fn set_sample_rate(&mut self, sr: f32) {
        self.rms_hf.set_sample_rate(sr);
        self.rms_ref.set_sample_rate(sr);
    }

    /// Returns `(band_sample, hf_db, ref_db)` for this sample. The band
    /// sample is what Monitor-Listen plays.
    #[inline]
    fn tick(&mut self, x: f32) -> (f32, f32, f32) {
        let mut hf = self.sc_hp.tick(x);
        if self.lp_active {
            hf = self.sc_lp.tick(&self.sc_lp_coeffs, hf);
        }
        let reference = self.ref_hp.tick(&self.ref_coeffs, x);
        (hf, self.rms_hf.tick(hf), self.rms_ref.tick(reference))
    }

    fn reset(&mut self) {
        self.sc_hp.reset();
        self.sc_lp.reset();
        self.ref_hp.reset();
        self.rms_hf.reset();
        self.rms_ref.reset();
    }
}

/// Integer-length delay ring whose active length slews one sample per
/// sample toward its target, so toggling lookahead never clicks — the
/// short glissando of a tape-speed change instead of a splice.
#[derive(Clone, Debug, Default)]
struct SlewDelay {
    buf: Vec<f32>,
    write: usize,
    len: usize,
    target: usize,
}

impl SlewDelay {
    fn set_sample_rate(&mut self, sr: f32) {
        let max = ((LOOKAHEAD_SEC * sr) as usize).max(1) + 1;
        self.buf = vec![0.0; max];
        self.write = 0;
        self.len = self.len.min(max - 1);
        self.target = self.target.min(max - 1);
    }

    fn set_target(&mut self, samples: usize) {
        self.target = samples.min(self.buf.len().saturating_sub(1));
    }

    /// Snap to the target length (used at activate, where a ramp would be
    /// audible as a wrong-latency first block).
    fn snap(&mut self) {
        self.len = self.target;
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        if self.len < self.target {
            self.len += 1;
        } else if self.len > self.target {
            self.len -= 1;
        }
        self.buf[self.write] = x;
        let read = (self.write + self.buf.len() - self.len) % self.buf.len();
        let y = self.buf[read];
        self.write = (self.write + 1) % self.buf.len();
        y
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
    }
}

/// The stereo de-esser core. Mono material uses the left lane only.
pub struct DeEsser {
    params: DeEssParams,
    sr: f32,

    det_l: Detector,
    det_r: Detector,

    shelf_l: RmHighShelf2,
    shelf_r: RmHighShelf2,
    bell_l: RmBell,
    bell_r: RmBell,

    dly_l: SlewDelay,
    dly_r: SlewDelay,

    // Derived per set_params.
    knee_db: f32,
    slope: f32,
    attack_coeff: f32,
    release_slew: f32,
    smooth_coeff: f32,

    // Ballistics state, dB of reduction (>= 0).
    gr_l: f32,
    gr_r: f32,
    gr_smooth_l: f32,
    gr_smooth_r: f32,

    // Block-level meters.
    meter_gr_db: f32,
    meter_excess_db: f32,
}

impl DeEsser {
    pub fn new(sr: f32) -> Self {
        let mut d = Self {
            params: DeEssParams::default(),
            sr,
            det_l: Detector::default(),
            det_r: Detector::default(),
            shelf_l: RmHighShelf2::default(),
            shelf_r: RmHighShelf2::default(),
            bell_l: RmBell::default(),
            bell_r: RmBell::default(),
            dly_l: SlewDelay::default(),
            dly_r: SlewDelay::default(),
            knee_db: 6.0,
            slope: 0.7,
            attack_coeff: 0.0,
            release_slew: 0.0,
            smooth_coeff: 0.0,
            gr_l: 0.0,
            gr_r: 0.0,
            gr_smooth_l: 0.0,
            gr_smooth_r: 0.0,
            meter_gr_db: 0.0,
            meter_excess_db: -60.0,
        };
        d.set_sample_rate(sr);
        d
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        self.sr = sr;
        self.det_l.set_sample_rate(sr);
        self.det_r.set_sample_rate(sr);
        self.dly_l.set_sample_rate(sr);
        self.dly_r.set_sample_rate(sr);
        self.smooth_coeff = 1.0 - (-1.0 / (SMOOTH_SEC * sr)).exp();
        let p = self.params;
        self.set_params(&p);
        self.dly_l.snap();
        self.dly_r.snap();
        self.reset();
    }

    /// Call once per block (or on any param change).
    pub fn set_params(&mut self, p: &DeEssParams) {
        self.params = *p;
        let sr = self.sr;

        self.det_l.configure(p.freq_hz, p.lp_hz, sr);
        self.det_r.configure(p.freq_hz, p.lp_hz, sr);

        self.shelf_l.set_freq(p.freq_hz, sr);
        self.shelf_r.set_freq(p.freq_hz, sr);
        // Bell centre: geometric middle of the sidechain band. With the LP
        // off, place it half an octave above the HP corner.
        let lp = if p.lp_hz < 19_999.0 { p.lp_hz.max(p.freq_hz * 1.25) } else { p.freq_hz * 1.414 };
        let centre = (p.freq_hz * lp).sqrt();
        // Q from the band span: wider band → wider notch.
        let octaves = (lp / p.freq_hz).log2().max(0.3);
        let q = (2.0 / octaves).clamp(1.0, 8.0);
        self.bell_l.set_freq(centre, q, sr);
        self.bell_r.set_freq(centre, q, sr);

        let d = p.detection.clamp(0.0, 1.0);
        self.knee_db = 12.0 * (1.0 - d);
        self.slope = 0.50 + 0.45 * d;

        self.attack_coeff = (-1.0 / (p.attack_ms.max(0.05) * 0.001 * sr)).exp();
        // Release: linear dB slew, "recover 10 dB in release_ms".
        self.release_slew = 10.0 / (p.release_ms.max(1.0) * 0.001 * sr);

        let look = if p.lookahead { ((LOOKAHEAD_SEC * sr) as usize).max(1) } else { 0 };
        self.dly_l.set_target(look);
        self.dly_r.set_target(look);
    }

    /// Samples of delay currently *targeted* — report this via
    /// `clap.latency`. The audio ramps to it within 2 ms of the change.
    pub fn latency_samples(&self) -> u32 {
        if self.params.lookahead {
            ((LOOKAHEAD_SEC * self.sr) as u32).max(1)
        } else {
            0
        }
    }

    /// Static curve: sibilant excess (dB over threshold) → target
    /// reduction (dB, >= 0), before the Range clamp.
    #[inline]
    fn static_gr(&self, excess: f32) -> f32 {
        let w = self.knee_db;
        let kneed = if w > 0.0 && excess.abs() <= w * 0.5 {
            let t = excess + w * 0.5;
            t * t / (2.0 * w)
        } else if excess > 0.0 {
            excess
        } else {
            0.0
        };
        self.slope * kneed
    }

    /// Per-sample detector → ballistics for one lane. Returns unsmoothed
    /// target-tracked reduction in dB.
    #[inline]
    fn track(&self, gr: f32, hf_db: f32, ref_db: f32) -> (f32, f32) {
        let level = match self.params.detect {
            DetectMode::Relative => hf_db - ref_db,
            DetectMode::Absolute => hf_db,
        };
        let excess = level - self.params.threshold_db;
        let gated = ref_db > GATE_DB;
        let target = if gated {
            self.static_gr(excess).min(self.params.range_db)
        } else {
            0.0
        };
        let next = if target > gr {
            // Attack: one-pole toward the (already program-dependent) target.
            target + (gr - target) * self.attack_coeff
        } else {
            // Release: linear dB slew — no exponential tail to dull the
            // vowel after the ess.
            (gr - self.release_slew).max(target)
        };
        (next, excess)
    }

    /// Process one stereo block in place. Detector reads the live input;
    /// the audio path runs through the lookahead delay.
    pub fn process_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let m_split = self.params.mode.clamp(0.0, 1.0);
        let m_wide = 1.0 - m_split;
        let link = self.params.link.clamp(0.0, 1.0);
        let shape = self.params.shape;
        let monitor = self.params.monitor;

        let mut peak_gr = 0.0f32;
        let mut peak_excess = -60.0f32;

        for i in 0..n {
            let (band_l, hf_l, ref_l) = self.det_l.tick(l[i]);
            let (band_r, hf_r, ref_r) = self.det_r.tick(r[i]);

            let (gl, el) = self.track(self.gr_l, hf_l, ref_l);
            let (gr_, er) = self.track(self.gr_r, hf_r, ref_r);
            self.gr_l = gl;
            self.gr_r = gr_;

            // Stereo link: blend each lane toward the deeper reduction.
            let deeper = gl.max(gr_);
            let use_l = gl + (deeper - gl) * link;
            let use_r = gr_ + (deeper - gr_) * link;

            // Post-ballistics smoother (also the zipper filter).
            self.gr_smooth_l += (use_l - self.gr_smooth_l) * self.smooth_coeff;
            self.gr_smooth_r += (use_r - self.gr_smooth_r) * self.smooth_coeff;

            let xl = self.dly_l.tick(l[i]);
            let xr = self.dly_r.tick(r[i]);

            let wl = Self::apply(shape, &mut self.shelf_l, &mut self.bell_l, xl, self.gr_smooth_l, m_wide, m_split);
            let wr = Self::apply(shape, &mut self.shelf_r, &mut self.bell_r, xr, self.gr_smooth_r, m_wide, m_split);

            match monitor {
                Monitor::Off => {
                    l[i] = wl;
                    r[i] = wr;
                }
                Monitor::Listen => {
                    l[i] = band_l;
                    r[i] = band_r;
                }
                Monitor::Delta => {
                    l[i] = xl - wl;
                    r[i] = xr - wr;
                }
            }

            if self.gr_smooth_l > peak_gr {
                peak_gr = self.gr_smooth_l;
            }
            if self.gr_smooth_r > peak_gr {
                peak_gr = self.gr_smooth_r;
            }
            if el > peak_excess {
                peak_excess = el;
            }
            if er > peak_excess {
                peak_excess = er;
            }
        }

        self.meter_gr_db = peak_gr;
        self.meter_excess_db = peak_excess;
    }

    /// Mono variant of [`Self::process_block`] — left lane only.
    pub fn process_mono(&mut self, buf: &mut [f32]) {
        let m_split = self.params.mode.clamp(0.0, 1.0);
        let m_wide = 1.0 - m_split;
        let shape = self.params.shape;
        let monitor = self.params.monitor;
        let mut peak_gr = 0.0f32;
        let mut peak_excess = -60.0f32;

        for x in buf.iter_mut() {
            let (band, hf, rf) = self.det_l.tick(*x);
            let (g, e) = self.track(self.gr_l, hf, rf);
            self.gr_l = g;
            self.gr_smooth_l += (g - self.gr_smooth_l) * self.smooth_coeff;
            let xd = self.dly_l.tick(*x);
            let w = Self::apply(shape, &mut self.shelf_l, &mut self.bell_l, xd, self.gr_smooth_l, m_wide, m_split);
            *x = match monitor {
                Monitor::Off => w,
                Monitor::Listen => band,
                Monitor::Delta => xd - w,
            };
            if self.gr_smooth_l > peak_gr {
                peak_gr = self.gr_smooth_l;
            }
            if e > peak_excess {
                peak_excess = e;
            }
        }
        self.meter_gr_db = peak_gr;
        self.meter_excess_db = peak_excess;
    }

    /// Apply `gr` dB of reduction, split between a broadband multiply and
    /// the shaper. Above the corner the two recombine to exactly
    /// `db_to_gain(-gr)` for every blend value, so the Wide↔Split sweep
    /// has no seam; at `gr == 0` every factor is exactly 1.0.
    #[inline]
    fn apply(
        shape: Shape,
        shelf: &mut RmHighShelf2,
        bell: &mut RmBell,
        x: f32,
        gr: f32,
        m_wide: f32,
        m_split: f32,
    ) -> f32 {
        let wide_gain = db_to_gain(-gr * m_wide);
        let split_db = gr * m_split;
        let x = x * wide_gain;
        match shape {
            Shape::Shelf => shelf.tick(x, db_to_gain(-split_db * 0.5)),
            Shape::Bell => bell.tick(x, db_to_gain(-split_db)),
        }
    }

    /// Peak smoothed gain reduction over the last block, dB (>= 0).
    pub fn gr_db(&self) -> f32 {
        self.meter_gr_db
    }

    /// Peak detector excess over the last block, dB relative to threshold.
    /// Positive = the detector is firing.
    pub fn excess_db(&self) -> f32 {
        self.meter_excess_db
    }

    pub fn reset(&mut self) {
        self.det_l.reset();
        self.det_r.reset();
        self.shelf_l.reset();
        self.shelf_r.reset();
        self.bell_l.reset();
        self.bell_r.reset();
        self.dly_l.reset();
        self.dly_r.reset();
        self.gr_l = 0.0;
        self.gr_r = 0.0;
        self.gr_smooth_l = 0.0;
        self.gr_smooth_r = 0.0;
        self.meter_gr_db = 0.0;
        self.meter_excess_db = -60.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Deterministic band-limited "ess": white-ish noise through a narrow
    /// band-pass at `f0`, RMS-normalised to `rms`.
    fn ess(len: usize, f0: f32, rms: f32) -> Vec<f32> {
        let coeffs = Biquad::bandpass(f0, 2.0, SR);
        let mut st = BiquadState::default();
        let mut seed = 0x1234_5678u32;
        let mut v: Vec<f32> = (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                st.tick(&coeffs, white)
            })
            .collect();
        let cur = (v.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
        let g = rms / cur.max(1e-12);
        v.iter_mut().for_each(|x| *x *= g);
        v
    }

    /// 200 Hz "vowel": sawtooth low-passed at 1.5 kHz, mimicking the
    /// formant roll-off of real voiced speech. (A raw sawtooth's 1/n
    /// harmonic series carries far more energy above 5 kHz than any vowel
    /// does, and reads as borderline sibilant to a tilt detector.)
    fn vowel(len: usize, rms: f32) -> Vec<f32> {
        let coeffs = Biquad::lowpass(1500.0, BUTTERWORTH_Q, SR);
        let mut st = BiquadState::default();
        let mut v: Vec<f32> = (0..len)
            .map(|i| {
                let ph = (200.0 * i as f32 / SR).fract();
                st.tick(&coeffs, 2.0 * ph - 1.0)
            })
            .collect();
        let cur = (v.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
        let g = rms / cur.max(1e-12);
        v.iter_mut().for_each(|x| *x *= g);
        v
    }

    fn run(d: &mut DeEsser, l: &[f32]) -> (Vec<f32>, f32) {
        let mut a = l.to_vec();
        let mut b = l.to_vec();
        d.process_block(&mut a, &mut b);
        let gr = d.gr_db();
        (a, gr)
    }

    #[test]
    fn vowel_passes_untouched_ess_gets_caught() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams { freq_hz: 5000.0, ..Default::default() });

        // Same RMS for both — the relative detector must separate them by
        // spectrum alone.
        let (_, gr_vowel) = run(&mut d, &vowel(24_000, 0.1));
        d.reset();
        let (_, gr_ess) = run(&mut d, &ess(24_000, 7000.0, 0.1));

        assert!(gr_vowel < 0.5, "vowel triggered {gr_vowel} dB");
        assert!(gr_ess > 4.0, "ess only triggered {gr_ess} dB");
    }

    #[test]
    fn relative_detection_is_level_independent() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams { freq_hz: 5000.0, ..Default::default() });

        let (_, gr_loud) = run(&mut d, &ess(24_000, 7000.0, 0.25));
        d.reset();
        let (_, gr_quiet) = run(&mut d, &ess(24_000, 7000.0, 0.005)); // −34 dB quieter

        assert!(gr_loud > 4.0 && gr_quiet > 4.0, "loud {gr_loud}, quiet {gr_quiet}");
        assert!((gr_loud - gr_quiet).abs() < 2.0, "not level-independent: {gr_loud} vs {gr_quiet}");
    }

    #[test]
    fn gate_ignores_room_tone() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams::default());
        // Hiss with high tilt but at −80 dBFS: gate must hold.
        let (_, gr) = run(&mut d, &ess(24_000, 7000.0, 1e-4));
        assert!(gr < 0.1, "gated material still reduced {gr} dB");
    }

    #[test]
    fn range_clamps_reduction() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams {
            freq_hz: 5000.0,
            range_db: 3.0,
            threshold_db: -40.0,
            ..Default::default()
        });
        let (_, gr) = run(&mut d, &ess(24_000, 7000.0, 0.2));
        assert!(gr <= 3.05, "range exceeded: {gr}");
        assert!(gr > 2.5, "range not reached: {gr}");
    }

    /// The wire guarantee end to end: silence through an idle instance, and
    /// (more strictly) audio through an instance whose detector never fires,
    /// must come out identical.
    #[test]
    fn idle_instance_is_a_wire() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams { range_db: 0.0, ..Default::default() });
        let x = vowel(4096, 0.2);
        let mut a = x.clone();
        let mut b = x.clone();
        d.process_block(&mut a, &mut b);
        assert_eq!(a, x, "range 0 must be bit-exact passthrough");
    }

    #[test]
    fn lookahead_reports_and_ramps() {
        let mut d = DeEsser::new(SR);
        assert_eq!(d.latency_samples(), 0);
        d.set_params(&DeEssParams { lookahead: true, ..Default::default() });
        assert_eq!(d.latency_samples(), 96);

        // Measured group delay after the ramp settles == reported.
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        d.process_block(&mut l, &mut r); // ramp happens in here (96 samples)
        let mut l2 = vec![0.0f32; 512];
        l2[0] = 1.0;
        let mut r2 = l2.clone();
        d.process_block(&mut l2, &mut r2);
        let peak = l2.iter().enumerate().max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap()).unwrap().0;
        assert_eq!(peak as u32, d.latency_samples(), "impulse landed at {peak}");
    }

    #[test]
    fn wide_split_blend_is_seamless_well_above_corner() {
        // The Wide↔Split identity `K_wide · K_shape == K_total` holds
        // exactly only in the shelf's stop band, so measure well above the
        // corner: ess at 14 kHz against a 3 kHz corner (2.2 octaves). Any
        // Mode value must then land within a fraction of a dB.
        let sig = ess(48_000, 14_000.0, 0.15);
        let mut outs = Vec::new();
        for &mode in &[0.0f32, 0.5, 1.0] {
            let mut d = DeEsser::new(SR);
            d.set_params(&DeEssParams { freq_hz: 3000.0, mode, ..Default::default() });
            let (y, _) = run(&mut d, &sig);
            let tail = &y[24_000..];
            let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
            outs.push(20.0 * rms.log10());
        }
        let spread = outs.iter().cloned().fold(f32::MIN, f32::max)
            - outs.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 1.0, "mode blend not seamless: {outs:?} dB");
    }

    #[test]
    fn release_recovers_after_the_ess() {
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams { freq_hz: 5000.0, release_ms: 50.0, ..Default::default() });

        let mut sig = ess(9600, 7000.0, 0.15);
        sig.extend(vowel(24_000, 0.1));
        let mut a = sig.clone();
        let mut b = sig.clone();
        d.process_block(&mut a, &mut b);
        // After ~350 ms of vowel, reduction must be fully released.
        let tail_out = &a[26_000..];
        let tail_in = &sig[26_000..];
        let rms_out = (tail_out.iter().map(|x| x * x).sum::<f32>() / tail_out.len() as f32).sqrt();
        let rms_in = (tail_in.iter().map(|x| x * x).sum::<f32>() / tail_in.len() as f32).sqrt();
        let diff_db = 20.0 * (rms_out / rms_in).log10();
        assert!(diff_db.abs() < 0.5, "vowel after ess still reduced {diff_db} dB");
    }

    #[test]
    fn stereo_link_matches_lanes() {
        // Wide mode so the reduction is broadband and measurable on the
        // low-frequency lane (a split shelf would, by design, leave a
        // vowel's spectrum almost untouched).
        let mut d = DeEsser::new(SR);
        d.set_params(&DeEssParams { freq_hz: 5000.0, link: 1.0, mode: 0.0, ..Default::default() });
        // Ess only in the left channel.
        let sl = ess(24_000, 7000.0, 0.15);
        let sr_ = vowel(24_000, 0.1);
        let mut l = sl.clone();
        let mut r = sr_.clone();
        d.process_block(&mut l, &mut r);
        // Linked: the right (vowel) lane must be reduced too.
        let rms_r_out = (r[12_000..].iter().map(|x| x * x).sum::<f32>() / 12_000.0).sqrt();
        let rms_r_in = (sr_[12_000..].iter().map(|x| x * x).sum::<f32>() / 12_000.0).sqrt();
        let red_db = -20.0 * (rms_r_out / rms_r_in).log10();
        assert!(red_db > 2.0, "linked lane only reduced {red_db} dB");

        // Unlinked: the vowel lane must stay put.
        let mut d2 = DeEsser::new(SR);
        d2.set_params(&DeEssParams { freq_hz: 5000.0, link: 0.0, mode: 0.0, ..Default::default() });
        let mut l2 = sl.clone();
        let mut r2 = sr_.clone();
        d2.process_block(&mut l2, &mut r2);
        let rms_r2 = (r2[12_000..].iter().map(|x| x * x).sum::<f32>() / 12_000.0).sqrt();
        let red2_db = -20.0 * (rms_r2 / rms_r_in).log10();
        assert!(red2_db < 0.5, "unlinked lane reduced {red2_db} dB");
    }

    #[test]
    fn program_dependent_attack_hits_harder_esses_faster() {
        // The RMS front-end climbs faster in dB for a larger step, so a
        // bigger *excess* is caught sooner — the 902's published 2 ms /
        // 600 µs behaviour. Absolute mode isolates this: in Relative mode
        // tilt is level-independent (by design), so loud and soft esses
        // attack identically there.
        fn time_to(d: &mut DeEsser, sig: &[f32], gr_target: f32) -> usize {
            let mut l = sig.to_vec();
            let mut r = sig.to_vec();
            for (i, chunk) in l.chunks_mut(16).zip(r.chunks_mut(16)).enumerate() {
                d.process_block(chunk.0, chunk.1);
                if d.gr_db() >= gr_target {
                    return i * 16;
                }
            }
            usize::MAX
        }
        let params = DeEssParams {
            freq_hz: 5000.0,
            threshold_db: -30.0,
            detect: DetectMode::Absolute,
            range_db: 20.0,
            ..Default::default()
        };
        let mut d = DeEsser::new(SR);
        d.set_params(&params);
        // ~−10 dBFS band → 20 dB over threshold.
        let hard = time_to(&mut d, &ess(24_000, 7000.0, 0.3), 2.0);
        let mut d2 = DeEsser::new(SR);
        d2.set_params(&params);
        // ~−26 dBFS band → 4 dB over threshold.
        let soft = time_to(&mut d2, &ess(24_000, 7000.0, 0.05), 2.0);
        assert!(
            hard < soft,
            "hard ess {hard} samples should beat soft {soft} samples"
        );
    }

    #[test]
    fn detection_knob_sharpens_the_curve() {
        // Just past threshold, a soft Detection setting reduces less than a
        // hard one (wider knee, shallower slope).
        let sig = ess(36_000, 7000.0, 0.05);
        let mut soft = DeEsser::new(SR);
        soft.set_params(&DeEssParams { freq_hz: 5000.0, threshold_db: -6.0, detection: 0.0, ..Default::default() });
        let (_, gr_soft) = run(&mut soft, &sig);
        let mut hard = DeEsser::new(SR);
        hard.set_params(&DeEssParams { freq_hz: 5000.0, threshold_db: -6.0, detection: 1.0, ..Default::default() });
        let (_, gr_hard) = run(&mut hard, &sig);
        assert!(
            gr_soft < gr_hard,
            "detection 0 ({gr_soft} dB) should be gentler than 1 ({gr_hard} dB)"
        );
    }
}
