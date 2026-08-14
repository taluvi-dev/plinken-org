//! Esscape — Plinken WCLAP de-esser.
//!
//! Program-adaptive sibilance reduction on the dbx 902's relative-threshold
//! detector, applied through Regalia–Mitra sections that are a bit-exact
//! wire at rest. DSP core lives in `plinken-dsp::deess`; this crate is the
//! CLAP glue: params, latency reporting, metering, and the display feeds
//! (scope buckets + 1/3-octave ess profile). Full design:
//! `plugins/com.plinken/esscape/DESIGN.md`.
//!
//! Latency contract: `latency_samples()` follows the Lookahead switch
//! (0 or 2 ms) and is reported honestly via `clap.latency` — delay
//! compensation is the host's job.

extern crate alloc;

use plinken_dsp::deess::{DeEssParams, DeEsser, DetectMode, Monitor, Shape};
use plinken_dsp::eq::{ThirdOctaveBank, THIRD_OCTAVE_BANDS};
use wclap_plugin::{
    init_plugin, ParamDef, Plugin, PluginDef, ProcessCtx, ProcessStatus, PARAM_IS_AUTOMATABLE,
    PARAM_IS_STEPPED,
};

static PLUGIN_DEF: PluginDef = PluginDef {
    id: b"com.plinken.esscape\0",
    name: b"Esscape\0",
    vendor: b"Plinken\0",
    url: b"https://plinken.org\0",
    version: b"0.1.0\0",
    description: b"Program-adaptive de-esser: relative (dbx 902-style) detection, transparent-at-rest split-band reduction, optional 2 ms lookahead (reported as latency). Place before the compressor.\0",
    features: &[b"audio-effect\0", b"de-esser\0", b"dynamics\0"],
    audio_inputs: 1,
    audio_outputs: 1,
    note_inputs: 0,
    ui_path: Some(b"/ui/index.html\0"),
};

// Param IDs — stable; saved automation depends on them.
const PID_FREQ: u32 = 0x0001;
const PID_LP: u32 = 0x0002;
const PID_THRESHOLD: u32 = 0x0003;
const PID_RANGE: u32 = 0x0004;
const PID_DETECTION: u32 = 0x0005;
const PID_ATTACK: u32 = 0x0006;
const PID_RELEASE: u32 = 0x0007;
const PID_MODE: u32 = 0x0008;
const PID_SHAPE: u32 = 0x0009;
const PID_DETECT: u32 = 0x000A;
const PID_LOOKAHEAD: u32 = 0x000B;
const PID_MONITOR: u32 = 0x000C;
const PID_LINK: u32 = 0x000D;

// Meter ids (readonly, pushed to the UI).
const PID_METER_GR: u32 = 0x1000;
const PID_METER_PEAK: u32 = 0x1001;
const PID_METER_EXCESS: u32 = 0x1002;
const PID_SAMPLE_RATE: u32 = 0x1003;

static PARAMS: &[ParamDef] = &[
    ParamDef { id: PID_FREQ, flags: PARAM_IS_AUTOMATABLE, name: b"Freq\0", module: b"Sidechain\0", min: 2000.0, max: 16000.0, default: 6500.0 },
    ParamDef { id: PID_LP, flags: PARAM_IS_AUTOMATABLE, name: b"Lp\0", module: b"Sidechain\0", min: 4000.0, max: 20000.0, default: 20000.0 },
    ParamDef { id: PID_THRESHOLD, flags: PARAM_IS_AUTOMATABLE, name: b"Threshold\0", module: b"\0", min: -45.0, max: 0.0, default: -14.0 },
    ParamDef { id: PID_RANGE, flags: PARAM_IS_AUTOMATABLE, name: b"Range\0", module: b"\0", min: 0.0, max: 20.0, default: 8.0 },
    ParamDef { id: PID_DETECTION, flags: PARAM_IS_AUTOMATABLE, name: b"Detection\0", module: b"\0", min: 0.0, max: 100.0, default: 50.0 },
    ParamDef { id: PID_ATTACK, flags: PARAM_IS_AUTOMATABLE, name: b"Attack\0", module: b"\0", min: 0.2, max: 5.0, default: 0.8 },
    ParamDef { id: PID_RELEASE, flags: PARAM_IS_AUTOMATABLE, name: b"Release\0", module: b"\0", min: 10.0, max: 250.0, default: 70.0 },
    ParamDef { id: PID_MODE, flags: PARAM_IS_AUTOMATABLE, name: b"Mode\0", module: b"\0", min: 0.0, max: 100.0, default: 100.0 },
    ParamDef { id: PID_SHAPE, flags: PARAM_IS_AUTOMATABLE | PARAM_IS_STEPPED, name: b"Shape\0", module: b"\0", min: 0.0, max: 1.0, default: 0.0 },
    ParamDef { id: PID_DETECT, flags: PARAM_IS_AUTOMATABLE | PARAM_IS_STEPPED, name: b"Detect\0", module: b"\0", min: 0.0, max: 1.0, default: 0.0 },
    ParamDef { id: PID_LOOKAHEAD, flags: PARAM_IS_AUTOMATABLE | PARAM_IS_STEPPED, name: b"Lookahead\0", module: b"\0", min: 0.0, max: 1.0, default: 0.0 },
    ParamDef { id: PID_MONITOR, flags: PARAM_IS_STEPPED, name: b"Monitor\0", module: b"\0", min: 0.0, max: 2.0, default: 0.0 },
    ParamDef { id: PID_LINK, flags: PARAM_IS_AUTOMATABLE, name: b"Link\0", module: b"\0", min: 0.0, max: 100.0, default: 100.0 },
];

fn amp_to_db(amp: f32) -> f32 {
    if amp <= 1.0e-9 {
        -120.0
    } else {
        20.0 * amp.log10()
    }
}

// ---------------------------------------------------------------------------
// CBOR message encoders (wasm → UI). Shapes match the decoders in
// widgets/cbor.mjs: a params map, and single-key byte-string blobs.
// ---------------------------------------------------------------------------

fn encode_params(buf: &mut [u8], pairs: &[(u32, f64)]) -> usize {
    if pairs.len() > 23 {
        return 0;
    }
    let needed = 1 + 1 + 6 + 1 + pairs.len() * 14;
    if buf.len() < needed {
        return 0;
    }
    let mut i = 0;
    buf[i] = 0xa1; i += 1;
    buf[i] = 0x66; i += 1;
    buf[i..i + 6].copy_from_slice(b"params"); i += 6;
    buf[i] = 0xa0 | (pairs.len() as u8); i += 1;
    for (id, v) in pairs {
        buf[i] = 0x1a; i += 1;
        buf[i..i + 4].copy_from_slice(&id.to_be_bytes()); i += 4;
        buf[i] = 0xfb; i += 1;
        buf[i..i + 8].copy_from_slice(&v.to_be_bytes()); i += 8;
    }
    i
}

/// `{ <key4>: <byte string of big-endian f32> }` — the `{"spec": …}` shape
/// the spectrum plugin established, reused for the ess-profile bands
/// (key `"spec"`) and the scope buckets (key `"scop"`).
fn encode_f32_blob(buf: &mut [u8], key4: &[u8; 4], values: &[f32]) -> usize {
    let payload = values.len() * 4;
    let needed = 1 + 1 + 4 + 3 + payload;
    if buf.len() < needed || payload > u16::MAX as usize {
        return 0;
    }
    let mut i = 0;
    buf[i] = 0xa1; i += 1;                       // map(1)
    buf[i] = 0x64; i += 1;                       // text(4)
    buf[i..i + 4].copy_from_slice(key4); i += 4;
    buf[i] = 0x59; i += 1;                       // bytes, u16 length
    buf[i..i + 2].copy_from_slice(&(payload as u16).to_be_bytes()); i += 2;
    for v in values {
        buf[i..i + 4].copy_from_slice(&v.to_be_bytes()); i += 4;
    }
    i
}

// ---------------------------------------------------------------------------
// Scope accumulator: fixed-rate (peak, gr) buckets for the time display.
// ---------------------------------------------------------------------------

/// Scope buckets accumulated between UI pushes. At ~240 buckets/s and a
/// 30 Hz push cadence this is 8 buckets per message.
const SCOPE_BUCKETS_PER_SEC: f32 = 240.0;
const SCOPE_MAX_BUCKETS: usize = 16;

struct Scope {
    /// Interleaved (input peak dBFS, gr dB) pairs.
    buckets: [f32; SCOPE_MAX_BUCKETS * 2],
    count: usize,
    cur_peak: f32,
    cur_gr: f32,
    samples_in_bucket: u32,
    samples_per_bucket: u32,
}

impl Scope {
    fn new() -> Self {
        Self {
            buckets: [0.0; SCOPE_MAX_BUCKETS * 2],
            count: 0,
            cur_peak: 0.0,
            cur_gr: 0.0,
            samples_in_bucket: 0,
            samples_per_bucket: 200,
        }
    }

    fn set_sample_rate(&mut self, sr: f32) {
        self.samples_per_bucket = ((sr / SCOPE_BUCKETS_PER_SEC) as u32).max(1);
    }

    /// Feed one block's input peak + gr, advancing buckets on schedule.
    fn push_block(&mut self, frames: u32, in_peak: f32, gr_db: f32) {
        if in_peak > self.cur_peak {
            self.cur_peak = in_peak;
        }
        if gr_db > self.cur_gr {
            self.cur_gr = gr_db;
        }
        self.samples_in_bucket += frames;
        while self.samples_in_bucket >= self.samples_per_bucket {
            self.samples_in_bucket -= self.samples_per_bucket;
            if self.count < SCOPE_MAX_BUCKETS {
                self.buckets[self.count * 2] = amp_to_db(self.cur_peak);
                self.buckets[self.count * 2 + 1] = self.cur_gr;
                self.count += 1;
            }
            self.cur_peak = 0.0;
            self.cur_gr = 0.0;
        }
    }

    fn take(&mut self) -> ([f32; SCOPE_MAX_BUCKETS * 2], usize) {
        let out = (self.buckets, self.count);
        self.count = 0;
        out
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

struct Esscape {
    core: DeEsser,
    params: DeEssParams,
    /// Raw stepped/percent param values as the host knows them.
    raw: [f64; 13],
    dirty: bool,

    bank: ThirdOctaveBank,
    scope: Scope,

    sample_rate: f32,
    meter_out_peak: f32,
    frame_count: u32,
    send_interval_frames: u32,
    sent_sample_rate: bool,
}

impl Esscape {
    fn raw_index(id: u32) -> Option<usize> {
        match id {
            PID_FREQ => Some(0),
            PID_LP => Some(1),
            PID_THRESHOLD => Some(2),
            PID_RANGE => Some(3),
            PID_DETECTION => Some(4),
            PID_ATTACK => Some(5),
            PID_RELEASE => Some(6),
            PID_MODE => Some(7),
            PID_SHAPE => Some(8),
            PID_DETECT => Some(9),
            PID_LOOKAHEAD => Some(10),
            PID_MONITOR => Some(11),
            PID_LINK => Some(12),
            _ => None,
        }
    }

    fn rebuild_params(&mut self) {
        let r = &self.raw;
        self.params = DeEssParams {
            freq_hz: r[0] as f32,
            lp_hz: r[1] as f32,
            threshold_db: r[2] as f32,
            range_db: r[3] as f32,
            detection: (r[4] as f32) * 0.01,
            attack_ms: r[5] as f32,
            release_ms: r[6] as f32,
            mode: (r[7] as f32) * 0.01,
            shape: if r[8] >= 0.5 { Shape::Bell } else { Shape::Shelf },
            detect: if r[9] >= 0.5 { DetectMode::Absolute } else { DetectMode::Relative },
            monitor: match r[11] as u32 {
                1 => Monitor::Listen,
                2 => Monitor::Delta,
                _ => Monitor::Off,
            },
            link: (r[12] as f32) * 0.01,
            lookahead: r[10] >= 0.5,
        };
    }
}

impl Plugin for Esscape {
    fn new() -> Self {
        let sr = 48_000.0_f32;
        let mut raw = [0.0f64; 13];
        for p in PARAMS {
            if let Some(i) = Self::raw_index(p.id) {
                raw[i] = p.default;
            }
        }
        let mut s = Self {
            core: DeEsser::new(sr),
            params: DeEssParams::default(),
            raw,
            dirty: true,
            bank: ThirdOctaveBank::new(sr),
            scope: Scope::new(),
            sample_rate: sr,
            meter_out_peak: 0.0,
            frame_count: 0,
            send_interval_frames: 1600,
            sent_sample_rate: false,
        };
        s.rebuild_params();
        s
    }

    fn activate(&mut self, sample_rate: f64, _max_frames: u32) {
        self.sample_rate = sample_rate as f32;
        self.core.set_sample_rate(self.sample_rate);
        self.bank.set_sample_rate(self.sample_rate);
        self.scope.set_sample_rate(self.sample_rate);
        self.send_interval_frames = (self.sample_rate / 30.0) as u32;
        self.dirty = true;
        self.sent_sample_rate = false;
    }

    fn reset(&mut self) {
        self.core.reset();
        self.bank.reset();
    }

    fn params() -> &'static [ParamDef] {
        PARAMS
    }

    fn get_param(&self, id: u32) -> f64 {
        Self::raw_index(id).map(|i| self.raw[i]).unwrap_or(0.0)
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if let Some(i) = Self::raw_index(id) {
            let def = &PARAMS[i];
            self.raw[i] = value.clamp(def.min, def.max);
            self.dirty = true;
        }
    }

    /// The lookahead delay is real latency the host must compensate.
    /// Follows the Lookahead switch: 0 when off, 2 ms worth when on.
    fn latency_samples(&self) -> u32 {
        self.core.latency_samples()
    }

    fn process(&mut self, ctx: &mut ProcessCtx) -> ProcessStatus {
        if self.dirty {
            self.rebuild_params();
            self.core.set_params(&self.params);
            self.dirty = false;
        }

        let mut n_processed: u32 = 0;
        let mut in_peak = 0.0f32;
        let mut out_peak = self.meter_out_peak;

        if ctx.input_channel_count() == 2 && ctx.output_channel_count() == 2 {
            if let Some(io) = ctx.stereo_io() {
                let wclap_plugin::StereoIo { input_l, input_r, output_l, output_r } = io;
                let n = input_l.len();
                n_processed = n as u32;
                for f in 0..n {
                    let m = input_l[f].abs().max(input_r[f].abs());
                    if m > in_peak {
                        in_peak = m;
                    }
                    self.bank.tick((input_l[f] + input_r[f]) * 0.5);
                    output_l[f] = input_l[f];
                    output_r[f] = input_r[f];
                }
                self.core.process_block(output_l, output_r);
                for f in 0..n {
                    let m = output_l[f].abs().max(output_r[f].abs());
                    if m > out_peak {
                        out_peak = m;
                    }
                }
            }
        }

        if n_processed == 0 {
            if let Some(io) = ctx.mono_io() {
                let wclap_plugin::MonoIo { input, output } = io;
                let n = input.len();
                n_processed = n as u32;
                for f in 0..n {
                    let m = input[f].abs();
                    if m > in_peak {
                        in_peak = m;
                    }
                    self.bank.tick(input[f]);
                    output[f] = input[f];
                }
                self.core.process_mono(output);
                for y in output.iter() {
                    let m = y.abs();
                    if m > out_peak {
                        out_peak = m;
                    }
                }
            }
        }

        self.meter_out_peak = out_peak;
        self.scope.push_block(n_processed, in_peak, self.core.gr_db());

        self.frame_count += n_processed;
        if self.frame_count >= self.send_interval_frames {
            self.frame_count = 0;

            // 1. Meter params (+ sample rate once, so the UI can place the
            //    log-frequency axis against the true Nyquist).
            let mut buf = [0u8; 96];
            let pairs = [
                (PID_METER_GR, (-self.core.gr_db()) as f64),
                (PID_METER_PEAK, amp_to_db(self.meter_out_peak) as f64),
                (PID_METER_EXCESS, self.core.excess_db() as f64),
                (PID_SAMPLE_RATE, self.sample_rate as f64),
            ];
            let n_pairs = if self.sent_sample_rate { 3 } else { 4 };
            self.sent_sample_rate = true;
            let len = encode_params(&mut buf, &pairs[..n_pairs]);
            if len > 0 {
                ctx.send_to_ui(&buf[..len]);
            }
            self.meter_out_peak *= 0.5;

            // 2. Ess-profile bands (dBFS peaks since last push).
            let peaks = self.bank.take_peaks();
            let mut bands_db = [0.0f32; THIRD_OCTAVE_BANDS];
            for (i, p) in peaks.iter().enumerate() {
                bands_db[i] = amp_to_db(*p);
            }
            let mut sbuf = [0u8; 12 + THIRD_OCTAVE_BANDS * 4];
            let slen = encode_f32_blob(&mut sbuf, b"spec", &bands_db);
            if slen > 0 {
                ctx.send_to_ui(&sbuf[..slen]);
            }

            // 3. Scope buckets: interleaved (in-peak dBFS, gr dB).
            let (buckets, count) = self.scope.take();
            if count > 0 {
                let mut obuf = [0u8; 12 + SCOPE_MAX_BUCKETS * 8];
                let olen = encode_f32_blob(&mut obuf, b"scop", &buckets[..count * 2]);
                if olen > 0 {
                    ctx.send_to_ui(&obuf[..olen]);
                }
            }
        }
        ProcessStatus::Continue
    }
}

#[no_mangle]
pub extern "C" fn _initialize() {
    init_plugin::<Esscape>(&PLUGIN_DEF);
}
