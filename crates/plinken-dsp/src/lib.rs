//! Plinken DSP — shared hand-rolled DSP primitives.
//!
//! Vendored from the private Plinken monorepo (`plinken-dsp`); this
//! copy in plinken-org is canonical from now on. Published under the
//! repo's MIT license (crate is dual MIT OR Apache-2.0).
//!
//! Extracted from the Synome synthesizer so every in-house plugin
//! (and, where it makes sense, the engines) builds on the same
//! proven building blocks instead of re-rolling them per plugin.
//!
//! Contents:
//! - oscillator with saw/pulse morphing, FM input and hard sync
//! - Moog ladder filter (2/4 pole, LP/BP/HP)
//! - ADSR envelope
//! - LFO (sine/triangle/saw/square/S&H) with onset delay
//! - white/pink noise
//! - effects: delay, comb+allpass reverb, chorus/phaser/flanger
//! - one-pole parameter smoother (zipper-free param changes)
//! - saturation helpers (`fast_tanh`, `soft_clip`) and `midi_to_freq`
//!
//! Crate rules (plugins/ARCHITECTURE.md, Decision 3): zero
//! dependencies, no platform code, no I/O, no allocation in process
//! paths — allocate in `new()` / `set_sample_rate()` only. Compiles
//! unchanged for `wasm32-unknown-unknown` and native targets.

pub mod deess;
pub mod envelope;
pub mod eq;
pub mod filter;
pub mod fx;
pub mod lfo;
pub mod math;
pub mod noise;
pub mod osc;
pub mod smoother;

pub use envelope::Envelope;
pub use filter::MoogFilter;
pub use fx::{Delay, ModulationFx, Reverb};
pub use lfo::Lfo;
pub use math::{fast_tanh, midi_to_freq, soft_clip};
pub use noise::Noise;
pub use osc::{OscMode, Oscillator};
pub use smoother::Smoother;
