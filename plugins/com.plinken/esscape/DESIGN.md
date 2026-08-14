# Esscape — `com.plinken.esscape` (design)

**Status:** design only — no code written yet. Research date 2026-08-14.

**Esscape** is a program-adaptive de-esser for the Plinken WCLAP shelf,
voiced to sit in the same chain as the SSL 4000-style EQ and channel
compressor that already live in `plinken-dsp` (private repo).

> The name is the product: the ess *escapes* the vocal, and the plugin is an
> escape route rather than a clamp. Display name **Esscape**, bundle id
> `com.plinken.esscape`, npm package `@plinken/esscape`, artifact
> `dist/esscape.wclap.tar.gz`. The CLAP feature tag stays the generic
> `"de-esser"` so the DAW picker files it under Dynamics without a host-side
> change.

Three references, deliberately:

* **dbx 902** — the character and the detector topology. The *right*
  historical partner for the SSL strip (see §1).
* **FabFilter Pro-DS** — the modern feature bar: split/wideband, sidechain
  HP+LP, listen mode, stereo link, fixed-latency lookahead.
* **Waves Sibilance** — the interaction model: a time-domain scope instead of
  a filter graph, a continuous Wide↔Split blend, a single Detection knob in
  place of ratio + knee, lookahead as a plain switch (§10.1, §11).

---

## 1. Why the dbx 902 is the correct reference

The private repo's `plinken-dsp/src/dynamics.rs` models the SL 611E channel
dynamics card, whose gain element is a **dbx 2151 VCA** fed by an AD536
true-RMS sidechain. The 902 de-esser is the same company's VCA + RMS-detector
lineage, built for the 900-series racks that sat beside those desks. Pairing
it with our SSL EQ/compressor isn't a mash-up — it's the rack that actually
existed.

More importantly, the 902's *detector* is the good idea, and it is exactly
what the brief asked for. From the manual ([dbx 902 owner's manual](https://adn.harmanpro.com/product_documents/documents/502_1323992524/902%20Owners%20Manual_original.pdf)):

> **Log-Domain Processing:** The 902 examines the differences **in dB**
> between the high frequency and full-bandwidth portions of the signal,
> allowing de-essing of signals which change in level by as much as 60 dB.
> Conventional de-essers require readjustment of their threshold control
> when a vocalist drops from singing voice to a whispering voice. By
> contrast, **the 902 does not even have a threshold control**.

Measured specs worth copying:

| Spec | dbx 902 |
|---|---|
| Attack rate | program-dependent: **2 ms** for 10 dB over, **600 µs** for 20 dB over (to 63 % GR) |
| Release rate | **925 dB/sec** (a *linear* dB slew, not an exponential tail) |
| De-ess crossover | **800 Hz – 8 kHz**, 12 dB/oct, "maximally flat, phase coherent" |
| Max attenuation | **0 – 20 dB** (the RANGE control) |
| Operating range | uniform over **−40 dBu … +24 dBu** input with no adjustment |
| Modes | NORMAL (broadband GR) / **HF ONLY** (band GR) |
| Controls | **two knobs**: FREQUENCY, RANGE |

That NORMAL / HF-ONLY pair is, 40 years early, exactly today's
**wideband / split-band** switch. And "no threshold control" is the
relative-threshold detector the brief specified.

Pro-DS confirms the same split from the modern side: *Single Vocal* uses an
intelligent detector that "splits sibilance from non-sibilance", *Allround*
"depends only on the frequency range … in combination with the Threshold
setting" — i.e. relative vs. absolute. We ship both.

---

## 2. Signal flow

```
                        ┌───────────────── DETECTOR (never delayed) ─────────────────┐
                        │                                                            │
 in ──┬──────────────►  ├─ LR4 HP @ Freq ─► LP @ Lp ─► RMS(τ=2ms) ──► log2 ─► hf_dB ─┤
      │                 │                                                            │──► tilt_dB = hf_dB − ref_dB
      │                 └─ BW2 HP @ 250Hz ────────────► RMS(τ=2ms) ──► log2 ─► ref_dB┤       (Relative)
      │                                                                              │    or hf_dB   (Absolute)
      │                                                                              ▼
      │                                                              gate (ref < −70 dBFS → 0)
      │                                                                              │
      │                                                              static curve: knee + slope from Detection
      │                                                                              │
      │                                                              ballistics: attack one-pole (dB)
      │                                                                          release linear slew (dB)
      │                                                                          clamp to Range
      │                                                                          smoother t63 = 0.2 ms
      │                                                                              │
      │                                                                       gr_dB ─┤
      ▼                                                                              ▼
  delay line (fixed 2 ms) ──[tap: 2 ms − Lookahead]──► shaper ────────────────────► out
                                                       ├ WIDE  : y = x · 2^(−gr·0.166096)
                                                       ├ SHELF : 2 × Regalia–Mitra HS @ Freq, each √K
                                                       └ BELL  : Regalia–Mitra peak @ √(Freq·Lp), gain K
```

The detector reads the **undelayed** input; the audio path is delayed. That
difference *is* the lookahead. See §6.

---

## 3. Detector

### 3.1 Sidechain filters

Two user controls, following Pro-DS ("You can choose any range between 2 kHz
and 20 kHz"), and matching the brief's "HP 2–12 kHz, LP 5–20 kHz or bypassed":

* **Freq (HP)** — 2 – 16 kHz, log. **4th-order Linkwitz–Riley** (two cascaded
  Butterworth biquads, Q = 0.7071). 24 dB/oct.
  *Why 4th order:* a 12 dB/oct HP at 6.5 kHz is still only −12 dB at 3.25 kHz,
  which is right where a bright vowel's F3 lives — that leakage is what makes
  cheap de-essers duck on "aah". At 24 dB/oct the same octave is −24 dB and
  the vowel stops voting.
* **Lp** — 4 – 20 kHz, log, **off at the top**. 2nd-order Butterworth,
  12 dB/oct. Keeps cymbal air / hiss / ultrasonic junk from biasing the
  detector. Internally clamped to `max(Lp, Freq·1.25)` and to `0.45·sr`.

Both are clamped against sample rate so a 44.1 kHz session can't be asked for
a 20 kHz corner.

### 3.2 Reference band

For **Relative** detection we need "everything else" to compare against. Not
the literal full band: on a vocal bus with bass bleed, low-frequency energy
inflates the reference and the de-esser never fires. So the reference is
**2nd-order Butterworth HP at 250 Hz** — "the voice band". A fixed constant,
not a control.

### 3.3 RMS averagers, matched

```
ms += (x·x − ms) · (1 − exp(−1/(τ·sr)))       τ = 2.0 ms, both detectors
level_dB = 3.0103 · fast_log2(ms + 1e-30)      # 10·log10(x) ≡ 3.0103·log2(x)
```

τ is **matched** between the HF and reference detectors and that matters more
than its exact value: if the reference were slower, every transient would
produce a false tilt spike and the de-esser would chatter on plosives. 2 ms
is short enough to catch an ess onset and long enough to average the noise
ripple of a fricative.

Program-dependent attack falls out of this for free and is the same mechanism
the SSL channel comp already uses: a fixed-τ mean-square averager climbs
faster in dB for a larger step, so a hard "ts" grabs sooner than a soft "s"
— reproducing the 902's 2 ms / 600 µs spec without faking it with a curve.

### 3.4 Relative vs Absolute

```
Relative (default, the 902):   excess = (hf_dB − ref_dB) − Threshold
Absolute (Pro-DS "Allround"):  excess =  hf_dB          − Threshold
```

Because the HF band is a subset of the reference band, `tilt = hf_dB − ref_dB`
is always ≤ 0. In practice:

| Material | tilt |
|---|---|
| sustained vowel | −25 … −35 dB |
| consonant-rich speech | −15 … −20 dB |
| **/s/, /ʃ/, /t͡s/** | **−6 … −1 dB** |

So a threshold of −30 … −3 dB is the whole useful span — which is exactly the
range the brief specified. That is not a coincidence; it is what a
spectral-tilt detector measures.

One `Threshold` param spans **−45 … 0 dB** and is read as tilt-dB in Relative
mode and dBFS in Absolute mode; both interpretations fit comfortably in that
range, so the automation lane stays single-scale. Value text changes with the
mode (`−14 dB rel` vs `−28 dBFS`).

### 3.5 Gate

`ref_dB < −70 dBFS` → target GR forced to 0 (and released out normally, not
snapped). Without it the relative detector happily de-esses room tone, whose
tilt is high and whose level is irrelevant.

---

## 4. Static curve and ballistics

```
d        = Detection / 100
W        = 12 · (1 − d)            knee width, 12 dB … 0 dB
slope    = 0.50 + 0.45 · d         0.50 … 0.95

knee(e)  = 0                       e < −W/2
         = (e + W/2)² / (2W)      |e| ≤ W/2      (W > 0)
         = e                       e > W/2
gr_target = min( Range, slope · knee(excess) )
```

* **Detection 0 … 100**, default **50** — the one knob that shapes the
  static curve, from broad-and-forgiving to tight-and-surgical. It replaces
  the ratio and knee controls a compressor would have; see §10.1 for why all
  four references agree that a de-esser doesn't want a ratio.
* **Range 0 … 20 dB**, matching the 902's RANGE exactly. Hard clamp; the
  0.2 ms smoother rounds the corner.
* **Attack 0.2 … 5 ms**, log, default **0.8 ms**. dB-domain one-pole.
  Below ~0.3 ms the 2 ms RMS window is the limiter, not the attack.
  Above ~3 ms you deliberately let the ess onset through — sometimes the
  right call for intelligibility.
* **Release 10 … 250 ms**, log, default **70 ms**. Implemented as the 902's
  **linear dB slew**, specified as "time to recover 10 dB":
  `slew = 10 / (release_ms · 0.001 · sr)` dB per sample. At 10.8 ms this
  reproduces the 902's 925 dB/s exactly. A linear release has no exponential
  tail, so a deep ess doesn't leave the following vowel dull.
  *Why not exponential:* the tail is what makes de-essers sound like they're
  "breathing" on the word after the ess.
* **Gain smoother** — dB-domain one-pole, t63 = 0.2 ms, after the clamp.
  Removes the release-slew corner and any param zipper, and (see §7) is what
  makes oversampling unnecessary.

---

## 5. The gain applicator — Regalia–Mitra, and why

The naive split-band de-esser runs a crossover, attenuates the high leg and
re-sums. That leaves the crossover's allpass phase rotation in the signal
**even when the de-esser is doing nothing**, which is why so many de-essers
"cost something" just by being inserted.

Instead, all three modes use structures whose gain `K` appears **only as two
scalar mix coefficients**, so:

1. `K` can be modulated **per sample** with zero coefficient recomputation
   (no `sin`/`cos`/`tan` in the audio loop, no zipper, no block-boundary
   stair-stepping under a 0.8 ms attack).
2. At `K = 1` the structure collapses to `y = x` **bit-exactly** — an
   idle de-esser is a wire. (`fast_exp2(0) == 1.0` exactly, by construction.)

The [Regalia–Mitra](https://www.researchgate.net/publication/3178187_Tunable_digital_frequency_response_equalization_filters)
allpass-based equaliser gives exactly that:

```
H(z) = (1+K)/2  +  (1−K)/2 · A(z)
```

* **A₁(z) = (a + z⁻¹)/(1 + a z⁻¹)**, `a = (tan(ω₀/2) − 1)/(tan(ω₀/2) + 1)`
  → A₁ = +1 at DC, −1 at Nyquist → **high shelf**: unity below ω₀, gain K above.
* **A₂(z) = (−c + d(1−c)z⁻¹ + z⁻²)/(1 + d(1−c)z⁻¹ − c z⁻²)**,
  `d = −cos ω₀`, `c = (tan(B/2) − 1)/(tan(B/2) + 1)`
  → A₂ = +1 at DC and Nyquist, −1 at ω₀ → **peaking bell**: unity away, gain K at ω₀.

Three modes, one gain variable:

| Mode | Structure | Character |
|---|---|---|
| **WIDE** | `y = x · K` | the 902's NORMAL. Most natural on a single close-mic'd vocal; the whole voice ducks a hair. |
| **SHELF** *(default)* | two cascaded A₁ shelves, each at `√K` | the 902's HF ONLY. 12 dB/oct transition — matching the 902's crossover slope — with a `K` floor above Freq. |
| **BELL** | one A₂ peak at `√(Freq·Lp)`, B from the Freq→Lp span | surgical. Leaves the air band above the ess intact; the modern "dynamic notch" sound. |

### 5.1 The SHELF mode is *exactly* a Linkwitz–Riley split — verified

The obvious objection to the shelf is "a shelf is gentler than a real
split-band de-esser". It isn't. Two cascaded A₁ stages at √K are
**numerically identical** to an LR2 (12 dB/oct, phase-coherent) crossover
split `LP² − K·HP²` — the exact topology the 902's HF-ONLY mode uses.
Verified at `f₀ = 6500 Hz`, `K = −8 dB`, `sr = 48 kHz`:

| f | 2 × Regalia–Mitra @ √K | LR2 split `LP² − K·HP²` |
|---|---|---|
| 1 000 Hz | −0.11 dB | −0.11 dB |
| 3 250 Hz | −1.03 dB | −1.03 dB |
| 4 600 Hz | −1.86 dB | −1.86 dB |
| **6 500 Hz** | **−3.11 dB** | **−3.11 dB** |
| 9 200 Hz | −4.72 dB | −4.72 dB |
| 13 000 Hz | −6.37 dB | −6.37 dB |
| 20 000 Hz | −7.81 dB | −7.81 dB |

Identical to every printed digit. So SHELF gives the classic split-band
response *and* an idle path that is a bare wire, *and* per-sample gain
modulation with no coefficient recomputation. The crossover form has none
of those three.

Note the consequence, and put it in the manual: **at `Freq` itself the shelf
sits at half of `Range`**, reaching full depth about an octave above. That is
not a shortcoming of this construction — an LR2 split does exactly the same
thing, because both legs are −6 dB at the corner. Set `Freq` at the *bottom*
of the sibilant band, not its centre. (BELL mode is the one that puts the
full `Range` precisely on `Freq`.)

**Known and embraced:** the gain-independent `c` makes Regalia–Mitra cuts
*narrower* than boosts of the same magnitude. For a unit that only ever cuts,
that reads as proportional-Q — gentle reduction stays broad, deep reduction
gets surgical. Measured (`f₀ = 8 kHz`, `Q = 2`), the half-depth bandwidth is
**3 385 Hz at 3 dB of reduction and 1 715 Hz at 15 dB**: the notch closes in
on the ess as it digs. It is the same law the G-series bells in `ssl_eq.rs`
already use, so the family voicing is consistent. Restoring textbook symmetry would
require `c` to depend on `K`, which would cost us the per-sample modulation —
a bad trade.

The bell/shelf corner is **derived from the sidechain filters**, not a
separate control — same rule Pro-DS uses ("the split frequency determined
automatically according to the chosen high-pass sidechain filtering
setting"). One less knob, and what you hear removed is always what triggered.

---

## 6. Lookahead — how much, and the constraint that decides it

### 6.1 How much a de-esser actually needs

Peak limiters need lookahead because a transient is sub-millisecond. **A
sibilant is not a transient.** /s/ and /ʃ/ are turbulent noise with a
gradual amplitude envelope; the phonetics literature works with fricative
stimuli on the order of ~150 ms total with ~110 ms rise, and affricate vs.
fricative identity is carried by *amplitude rise slope* precisely because
fricatives rise slowly.

With τ = 2 ms and an 0.8 ms attack, full gain reduction lands ≈ 2–3 ms after
onset. Against a 100 ms ess, 3 ms of unreduced material is ~3 % of the energy
— inaudible as level. What *is* audible is the affricate case: a /t͡s/ or
/t͡ʃ/ starts with a stop burst whose first 2–3 ms is genuinely spiky. That's
the case lookahead buys you.

Past ~3 ms, lookahead starts costing more than it buys: the gain begins
falling **before** the ess, so the tail of the preceding vowel gets ducked.
On a shelf that's a subtle HF dip; on WIDE it's an audible pre-dip that
reads as pumping.

> Pro-DS offers up to 15 ms and calls ~10 ms "optimal". We disagree for
> SHELF/WIDE and cap at 2 ms. If you want the long setting, `MAX_LOOKAHEAD_MS`
> is a one-line change — but it is paid for in constant latency (§6.2), which
> is a much more expensive currency in our host than it is in theirs.

**Recommendation: Off / 1 / 2 / 5 ms, default Off.** 1–2 ms is the sweet
spot; 5 ms is there for aggressive surgical work on already-recorded
material, with a note in the manual that it pre-ducks.

### 6.2 The rule: report the truth, let the host compensate

This is a **WCLAP plugin for an open format** — we do not know the host. So
the `Lookahead` switch does the plain, correct CLAP thing:

```
Lookahead = Off | 1 ms | 2 ms | 5 ms
latency_samples() = round(lookahead_ms · 0.001 · sr)      // 0, 48, 96, 240 @48k
```

The delay line is allocated at `activate()` for the maximum (5 ms) and the
active length follows the param; `latency_samples()` reports the active
length. PDC is the host's job. **Default is `Off`** — a plugin that silently
adds latency the moment you insert it is rude, and §6.1 says the honest
answer is that a de-esser barely needs lookahead anyway.

Changing the switch mid-stream ramps the delay length over ~5 ms rather than
jumping it, so the audio doesn't click while the host re-negotiates.

### 6.3 Known gap in *our* host (a host bug, not a plugin constraint)

Worth writing down because it will bite when this plugin is loaded in the
Plinken engines, and because the fix is small:

* `crates/wclap-host/src/host_stubs.rs` — `_wclap_host_get_extension` returns
  a pointer **only for `clap.webview`**; everything else returns 0. So a
  plugin can never obtain `clap_host_latency` and can never call
  `latency.changed()`.
* `_wclap_host_request_restart` is a **no-op** (`touch()` and return).
* Private repo, web engine: `refresh_track_latency()` re-queries `clap.latency`
  only on slot add/remove/bind — never on a param change.
* Private repo, `plinken-run`: `reported_latency()` is *"Captured once per
  instance … Dynamic latency changes (host `latency.changed` callbacks) are
  not tracked yet."*

So in the Plinken engines specifically, moving the `Lookahead` switch while a
project is playing will leave PDC compensating the *previous* value until the
slot is re-bound. The plugin is correct; the host needs to catch up.

**Fix (separate work, not this plugin):** teach `crates/wclap-host` to hand
out a real `clap_host_latency` and make `request_restart` re-query, then wire
`refresh_track_latency` to it in the web engine. Small, and it unblocks every
future variable-latency plugin. Until then, the practical guidance is: set
`Lookahead` before you hit play.

---

## 7. Things we deliberately do *not* do

* **No oversampling.** The only nonlinearity is the gain multiply. The
  0.2 ms dB-domain smoother bandlimits the gain signal to roughly 800 Hz, so
  modulation products sit within ±800 Hz of the sibilant band. A 20 kHz
  component at 44.1 kHz produces sidebands at 20.8 kHz — still under Nyquist.
  Pro-DS needs the option because it allows far faster gain movement; we
  don't. (If `MAX_LOOKAHEAD_MS` or the smoother ever changes, re-check this.)
* **No `Box<dyn>` anywhere** — root `CLAUDE.md` gotcha #3; LTO drops the
  vtable entries and the first call traps with `null function`.
* **No makeup gain.** The unit only ever attenuates.
* **No mid/side.** Pro-DS has it; it needs a matrix and two more params.
  v2 if asked for.

---

## 8. Stereo

Per-channel detectors (both bands), then:

```
gr_link  = max(gr_L, gr_R)
gr_ch    = lerp(gr_ch, gr_link, Link)          Link ∈ [0, 1], default 1.0
```

Default is fully linked — a de-esser that moves the stereo image on every
"s" is worse than the sibilance. The control exists because Pro-DS has it and
because a wide double-tracked vocal occasionally wants it.

---

## 9. Metering and the analyser

Pushed to the UI at 30 Hz via `ctx.send_to_ui()` (CBOR `{params:{id:val}}`,
same encoder as the compressor):

| id | value |
|---|---|
| `0x1000` | GR, dB (negative) — **the reduction meter** |
| `0x1001` | output peak, dBFS |
| `0x1002` | detector excess, dB (drives the "sibilance activity" ring) |
| `0x1010…0x1019` | 10 × 1/3-octave band level (peak-hold since last push) |

The band bank is 10 ISO 1/3-octave biquads (2 k … 16 k) on the mono sum —
~50 mults/frame, i.e. free. The plugin sends raw band peaks; the **UI** keeps
the ess-vs-baseline accumulators, so the "learn" logic can be tuned without a
wasm rebuild.

**Learn** button: the UI already has the band profile *and* the GR stream, so
it computes `ess_energy[b] / baseline_energy[b]`, takes the argmax, and writes
`Freq` — no plugin-side support needed at all.

---

## 10. Parameters

| PID | Name | Range | Default | Scale | Flags |
|---|---|---|---|---|---|
| `0x0001` | Freq (sidechain HP) | 2 000 – 16 000 Hz | 6 500 | log | auto |
| `0x0002` | Lp (sidechain LP) | 4 000 – 20 000 Hz (`off` at max) | 20 000 | log | auto |
| `0x0003` | Threshold | −45 – 0 dB | −14 | lin | auto |
| `0x0004` | Range | 0 – 20 dB | 8 | lin | auto |
| `0x0005` | Detection | 0 – 100 | 50 | lin | auto |
| `0x0006` | Attack | 0.2 – 5 ms | 0.8 | log | auto |
| `0x0007` | Release | 10 – 250 ms | 70 | log | auto |
| `0x0008` | Mode (Wide ↔ Split) | 0 – 100 | 100 | lin | auto |
| `0x0009` | Shape | 0 Shelf / 1 Bell | 0 | step | auto, stepped |
| `0x000A` | Detect | 0 Relative / 1 Absolute | 0 | step | auto, stepped |
| `0x000B` | Lookahead | 0 Off / 1 On (2 ms) | 0 | step | auto, stepped |
| `0x000C` | Monitor | 0 Off / 1 Listen / 2 Delta | 0 | step | auto, stepped |
| `0x000D` | Link | 0 – 100 % | 100 | lin | auto |

13 params (EQ has 10, compressor 6). `clap.state` persistence is the
scaffold's generic param dump — nothing to implement.

### 10.1 Three params that came from Waves Sibilance

**`Detection` replaces Ratio + Knee.** Sibilance has a single 0–100 knob with
a soft-ramp icon at one end and a hard-step icon at the other. That is the
better control: one knob sweeping the static curve from forgiving to
surgical.

```
knee_dB = 12 · (1 − d)              d = Detection/100  →  12 dB … 0 dB
slope   = 0.50 + 0.45 · d                              →  0.50 … 0.95
```

This also settles the ratio question: **none of the four references has a
ratio control** — not the 902 (FREQUENCY + RANGE only), not Pro-DS
(Threshold + Range), not R-DeEsser (Threshold + Range), not Sibilance
(Threshold + Range + Detection). Cut.

**`Mode` is a continuous Wide↔Split blend, not a switch.** Sibilance's MODE
knob sweeps between the two rather than toggling, and the blend is free in
our structure — split the reduction between a broadband multiply and the
shaper:

```
m = Mode/100
K_wide  = 2^(−gr · m′ · 0.166096)   m′ = 1 − m     applied to the whole signal
K_shape = 2^(−gr · m  · 0.166096)                  applied by shelf/bell
```

Above `Freq` the two multiply back to exactly `2^(−gr·0.166096)` for every
value of `m`, so the sweep has no discontinuity and full-Wide / full-Split
are the exact endpoints. At `gr = 0` both are 1.0 and the whole path is a
wire, as before.

**`Lookahead` is a plain on/off**, as Sibilance has it — `On` = 2 ms,
reported honestly via `latency_samples()`. §6.1 says 1–2 ms is where the
benefit lives, so a switch is the whole useful range; a 4-way selector was
false precision.

**Monitor** is the setting-up tool and the one feature neither reference has
in this form:

* **Listen** — hear the sidechain band. Sweep `Freq` until the ess is loudest.
* **Delta** — hear `delayed_input − output`, i.e. *exactly what is being
  removed*. If you hear vowel in the delta, back off. This is the single
  fastest way to set a de-esser and it costs one subtract.

---

## 11. UI

`widgets/` only (`pot.mjs`, `meter.mjs`, `transport.mjs`) plus one canvas.
Tokens from `widgets.css` — Unbounded/Courier Prime, `--accent #8691da`,
`--accent-purple #925db3`, `--meter-fill-gr #d49a3a`.

### Expanded — 660 × 400

Two displays, stacked. The **scope** is the hero; the **analyser** is the
map. That ordering comes straight from Waves Sibilance, which throws the
frequency graph away entirely and shows only time — because the question you
actually have while de-essing is *"which syllables did it catch, and how
hard?"*, not *"what shape is my filter?"*.

```
┌────────────────────────────── ESSCAPE ────────────────────────────────┐
│ ┌────────────────────────────────────────────────┐  ╭──────────╮      │
│ │  0 ─────╮──────╮───╮──────────╮────╮────── 0   │  │  THRESH  │  ┌─┐ │
│ │          ╰─╯    ╰──╯ ╰╯        ╰────╯  GR trace│  │  ╭────╮  │  │O│ │
│ │  ▁▄█▅▂▃██▅▂▁▄███▅▂▁▃██▆▃▁▂▅█▇▄▂  waveform,     │  │  │−14 │◜ │  │U│ │
│ │      ▓▓      ▓▓▓    ▓▓     ▓▓▓   esses in amber│  │  ╰────╯  │  │T│ │
│ └────────────────────────────────────────────────┘  │  −5.4 dB │  └─┘ │
│ ┌────────────────────────────────────────────────┐  ╰──────────╯      │
│ │ 1k   2k    5k   10k  20k        Atten: −11.4   │   GR arc + range   │
│ │      ▁▂▄██▇▅▂▁   1/3-oct ess profile           │   bracket on dial  │
│ │ ─────────╲____╱──  live attenuation curve      │                    │
│ │ ▏HP           ▕LP   draggable   ░░ range shade │       [ LEARN ]    │
│ └────────────────────────────────────────────────┘                    │
│ (◯)Freq (◯)Lp (◯)Range (◯)Detect (◯)Atk (◯)Rel (◯)Mode (◯)Link        │
│  SHAPE[SHELF|BELL]  DETECT[REL|ABS]  LOOKAHEAD[◉]  MONITOR[—|SC|Δ]    │
└───────────────────────────────────────────────────────────────────────┘
```

**Lane 1 — the scope** (Sibilance's idea, and the best one in any of the
four references):

* **GR trace** hanging from a `0` baseline, scrolling, ~2 s window.
* **Waveform** below it, with the frames where GR was active drawn in amber
  **in place**. You see the esses light up inside the performance.
* Fed by a `{"scope": <f32 blob>}` message — 8 buckets of `(peak, gr)` per
  30 Hz push = 240 buckets/s, 1 px each, so a 480 px lane holds ~2 s. Same
  encoder shape as the spectrum plugin's `{"spec": …}`, which already has a
  decoder in `widgets/cbor.mjs`.

**Lane 2 — the analyser.** Log-f x-axis, 1–20 kHz:

* **1/3-octave bars** coloured by *ess weight* — how much louder that band is
  during GR-active frames than during quiet ones. This is a picture of where
  this singer's sibilance actually lives, not a generic spectrum.
  **This is the one thing none of the three references draw.** R-DeEsser,
  Pro-DS and the 902 all show you the *filter* and make you find the frequency
  by ear with sidechain-listen. Showing the signal is our differentiator, and
  it costs 10 biquads.
* **Live attenuation curve** — the shaper's actual `|H(f)|` redrawn every
  frame at the current GR. The notch visibly opens and closes on each "s".
  At rest it is a dead-flat line, which is literally true.
* **Range shading** — the region the live curve can never leave, so an
  abstract dB number becomes a visible ceiling.
* **Attenuation peak-hold**, numeric, top-left of the canvas. Click to clear.
* **Draggable HP/LP handles** setting `Freq` / `Lp` directly.
* **GR history strip** along the bottom, ~3 s scrolling.

The trace vocabulary borrows from the [Waves R-DeEsser graph](https://assets.wavescdn.com/pdf/plugins/renaissance-deesser.pdf)
— green = sidechain range, red = crossover limit, yellow = live attenuation,
purple shading = the Range ceiling, numeric peak-hold at top, click to clear.
It is a well-solved information design; no reason to invent a different one.
Gain reduction is also drawn *on the Threshold control*, as R-DeEsser does:
"where is the line" and "how hard am I hitting it" belong in one glance.

### Two new widgets

* **`segment.mjs`** — a segmented button row for stepped params. Reusable;
  the saturator's Type knob wants it too. Reads far better than a pot with a
  `format` lookup for a 2- or 3-way.
* **`gr-pot.mjs`** — a `Pot` with a **live gain-reduction arc** wrapped
  around its right side and a **draggable Range bracket** clamped onto that
  arc, the way Sibilance's threshold dial works. Threshold, current GR and
  the Range ceiling in one cluster. This is what makes the compact face fit.

### Compact — 380 × 56 (the plugin-strip face)

```
[ESSCAPE]   ╭────╮   ▁▂▅▂▁    ╮──╮─╮────   ▮
            │−14 │◜  ess prof   GR trace   GR
            ╰────╯
          thresh+GR+range
```

One dial does three jobs (threshold, GR, range), so the strip face gets both
displays: the ⅓-octave sparkline **and** a miniature GR trace. That pairing
is the tile's signature — it's what makes Esscape identifiable at a glance
among six grey strips in the rack.

Geometry matches the compressor exactly (`--pot-size: 32px`,
`--meter-w: 8px`, `--meter-h: 36px`, labels/readouts hidden) so the rack
stays visually even.

`PluginChainTile.svelte` caps the compact iframe at `max-height: 80px` and
sizes the tile to `compact_size.width + 26`, so 380 × 56 is the safe target.

---

## 12. Where the code goes

Reusable DSP into **`crates/plinken-dsp`** (zero-dep, wasm32 + native from
one source, `cargo test` runs natively):

* `math.rs` — port `fast_log2` / `fast_exp2` from the private `plinken-dsp`
  (max rel. error 2e-4 ≈ 0.002 dB; **exact at integers**, so `fast_exp2(0) == 1.0`
  and the idle-is-a-wire guarantee holds).
* `filter.rs` — `BiquadRbj` (Butterworth HP/LP), `Lr4Hp`, `RegaliaShelf`,
  `RegaliaBell`, `ThirdOctaveBank`.
* `dynamics.rs` (new) — `RmsDetector`, `SibilanceDetector`, `DeEsserCore`.

The plugin crate stays thin: `PluginDef`, params, ring buffer, metering,
`ctx` plumbing.

### Tests (native, in `plinken-dsp`)

* shelf/bell at `K = 1` are **bit-exact** pass-through (`y == x`, not `≈`).
* shelf: |H| = 1 at DC, = K at Nyquist, = √K at the corner.
* bell: |H| = K at ω₀, → 1 at ±3 octaves.
* LR4 HP: −6 dB at fc, −24 dB/oct asymptote.
* RMS detector converges to the analytic RMS of a sine within 0.1 dB.
* **End-to-end:** synthesise a 200 Hz sawtooth "vowel" then a 7 kHz
  bandpassed-noise "ess" at equal RMS. Assert GR < 0.5 dB through the vowel,
  > 5 dB within 5 ms of the ess, and that the reported latency equals the
  measured group delay of an impulse.
* Gate: −80 dBFS noise produces zero GR.

---

## 13. Build & ship

Copy `plugins/com.plinken/vocal-limiter/` — including `build.rs` verbatim
(`--export-table` **and** `--growable-table`; root `CLAUDE.md` gotcha #1).

```sh
pnpm --filter @plinken/esscape build          # cargo → wasm → bundle-wclap.mjs
node scripts/build-shelf.mjs                   # regenerate shelf.json
```

Add both crates to the root `Cargo.toml` workspace members. `features` tags
`["audio-effect", "de-esser", "dynamics"]` so it lands in the DAW picker's
Dynamics bucket with no host-side change.

Chain placement, per the house guide (`seed-content/skills/daw/mixing-chain.md`):
**subtractive EQ → de-esser → compressor**. Worth a line in the description,
because putting it after the SSL comp means the comp's own gain reduction has
already pushed the sibilance up.

---

## 14. Open questions

1. **Does `Link` earn its slot?** First on the chopping block if the panel
   gets crowded — all four references either lack it or default it to fully
   linked, which is what a de-esser wants anyway.
2. **`Lp` default: off, or 12 kHz?** Off is honest (the detector sees the
   whole top end); 12 kHz keeps cymbal air out of the tilt on bus material.
   Shipping `off`; revisit after listening.

## Sources

* [dbx 902 de-Esser owner's manual](https://adn.harmanpro.com/product_documents/documents/502_1323992524/902%20Owners%20Manual_original.pdf) — specifications page (attack/release rates, crossover, RANGE, filter type, modes)
* [Lindell Audio 902 De-Esser — Plugin Alliance](https://www.plugin-alliance.com/products/902-de-esser) — HP/FBW RMS-comparator description
* [FabFilter Pro-DS — Basic controls](https://www.fabfilter.com/help/pro-ds/using/basiccontrols) — 2–20 kHz sidechain range, Single Vocal vs Allround
* [FabFilter Pro-DS — Advanced controls](https://www.fabfilter.com/help/pro-ds/using/advancedcontrols) — 15 ms lookahead & fixed latency, stereo link 0–100 %, auto split frequency
* [Waves Renaissance DeEsser user guide](https://assets.wavescdn.com/pdf/plugins/renaissance-deesser.pdf) — graph trace anatomy, adaptive threshold (−80…0, def −40), Range (−48…0, def −16), Freq 2–16 kHz (def 5506), HP/band-pass sidechain, Wideband/Split (def Split), phase-compensated crossover
* [Waves Sibilance](https://assets.wavescdn.com/pdf/plugins/sibilance.pdf) — scope-first display (GR trace over waveform with sibilants marked), Detection knob, continuous Wide↔Split Mode, Lookahead toggle, Range as a bracket on the threshold arc
* [Regalia & Mitra, *Tunable digital frequency response equalization filters*](https://www.researchgate.net/publication/3178187_Tunable_digital_frequency_response_equalization_filters) — the allpass structure
* [Jongman, *Phonetics of Fricatives*](https://kuppl.ku.edu/sites/kuppl/files/documents/publications/Jongman%20OREL%202024%20Phonetics%20of%20Fricatives.pdf) — fricative duration / rise-slope
* [Acoustic–phonetic mechanisms of adaptation in sibilant fricative perception](https://link.springer.com/article/10.3758/s13414-019-01894-2) — sibilant stimulus envelopes (150 ms / 110 ms rise)
* [Evaluating the spectral distinction between sibilant fricatives](https://pmc.ncbi.nlm.nih.gov/articles/PMC3027155/) — spectral peak / CoG, sex differences
* Private repo: `plinken-dsp/src/dynamics.rs` (SSL 611E / dbx 2151 lineage), `plinken-dsp/src/ssl_eq.rs` (E/G curve laws), `docs/plans/track-delay-and-pdc.md` (PDC state of play)
