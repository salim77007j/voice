# Micro-Vocal Lab — V2 Strategy

**Phase 8.1 deliverable** · Status: APPROVED · Date: 2026-10-05
Baseline: `main` = `v1.1.1` (`9c854cc`) — 217 tests green (re-verified on this tree:
217 passed / 0 failed / 5 ignored), clippy + fmt clean, 5-OS CI green.

Mission: transform Micro-Vocal Lab from a good tool into a world-class, competitive
product — the best free, lightweight, privacy-first vocal processor of 2026,
genuinely rivaling Melodyne, iZotope RX, Auto-Tune Pro and Adobe Podcast on the
axes where they can be beaten: **price ($0), privacy (100 % local), size
(< 50 MB), speed (36 ms cold start), and — by the end of V2 — depth and quality.**

---

## 1. Where we actually stand (honest audit)

### 1.1 Assets we keep and build on

| Asset | v1.1.1 state | Why it matters for V2 |
|---|---|---|
| **DSP spine** | identity-locked phase vocoder (Laroche–Dolson), rubato sinc resampler, cepstral true-envelope formant warper, classifier-gated breath engine, 4×-oversampled true-peak limiter | Every V2 module plugs into an existing, tested STFT/analysis spine — we are not starting a DSP engine from zero |
| **Analysis layer** | YIN F0 + MPM-style voicing, spectral features, Voiced/Sibilant/Breath/Silence classifier, transient detector | Auto-Tune, de-esser, vocal rider and de-click are all *consumers* of analysis we already run per frame |
| **Robustness record** | 1000-file fuzz zero panics, import matrix 8–192 kHz × 6 formats, device fallback chains + Windows privacy diagnostic, RefCell re-entrancy fixed structurally | The crash-safety reputation is the brand. Every new module inherits the invariant discipline (bit-exact neutral bypass, no panics in the audio path) |
| **Performance** | cold start 36 ms, preview 10.7 ms algorithmic latency, 22.7× realtime render, ≤ 22.4 MiB RSS, 21.6 MB binary | The **structural moat** vs RX (multi-GB install) and Adobe Podcast (upload + cloud queue). V2 must not lose it |
| **Studio UI** | Audioprecise Pro rack (gunmetal + gold/blue two-hue discipline, knobs, LED meters, 40-band RTA), EN + Arabic RTL | The design language scales horizontally: V2 modules become new channel strips / rack tabs in the same visual system |
| **Pipeline discipline** | preview and render share one algorithm with two profiles; golden-sample CI regression (bit-exact on fixture platform, ≤ −80 dBFS cross-platform) | "What you hear is what you export" survives V2 only if every new module honors it — this is a hard acceptance gate below |

### 1.2 Hard gaps (why v1.1.1 is not yet competitive)

1. **Three parameters.** Melodyne exposes per-note editing of pitch/timing/
   volume/formants; RX ships a dozen repair modules; Auto-Tune exposes key,
   scale, retune speed, formant, flex-tune. We ship pitch / air / tract.
   *Depth of chain is the single largest competitive gap.*
2. **No repair suite.** No de-noise, de-reverb, de-click. RX's Dialogue Isolate +
   combined De-noise/De-reverb is the reason it owns post-production.
3. **No pitch correction.** Auto-Tune's core function — snap F0 to a scale in
   real time — is absent, even though our YIN pitch track already computes the
   input signal for it every frame.
4. **No dynamics/mastering.** No EQ, compressor, LUFS metering, dither.
   Podcasters (our most reachable audience) finish a chain in Adobe Podcast
   with one click; we give them nothing comparable.
5. **Playhead desync bug** during playback (user-visible; scheduled fix 8.2).
6. **Single track, single preset, no undo.** Session state is one audio buffer +
   3 floats; a producer's mental model (chain, A/B, snapshots, recall) does not
   exist yet.
7. **Exports stop at WAV/MP3.** FLAC is the lossless-compressed standard for
   distribution; OGG Vorbis for web/podcasts.
8. **Pitch-shifting quality ceiling.** The PV path is artifact-controlled at
   moderate shifts but is not "zero artifacts on ±12 st" on vibrato-heavy and
   transient-heavy material (transient smearing is inherent to PV; the current
   transient detector gates but does not *preserve* transients).

### 1.3 Non-goals for v2.0.0 (explicit, so they stop consuming attention)

- **Polyphonic note editing (Melodyne DNA).** Requires note-level polyphonic
  transcription — a research project; not a feature gap we can close in one
  version. Monophonic voice is our stated scope (it is the product's name).
- **Cloud anything.** No accounts, no servers, no telemetry, no "enhance in the
  cloud". This is the privacy moat vs Adobe Podcast — non-negotiable.
- **Plugin formats (VST3/AU/AAX).** The product is a standalone application;
  a plugin wrapper is a post-2.0 conversation (and a licensing one — GPL +
  VST3 SDK is workable but AAX is not).
- **Bundled neural models.** See §4.1 — the architecture reserves a slot, ships
  classical in 2.0.0.

---

## 2. Competitive landscape (2026 research)

Researched 2026-10-05 (web sources: vendor pages, reviews, forums — key claims
paraphrased; prices as listed by vendors/retailers at research time).

### 2.1 The competitors

| Product | Price | Core moat | Weakness we exploit |
|---|---|---|---|
| **Celemony Melodyne 5** (Essential→Studio) | €99–€699 | DNA Direct Note Access (edit notes inside polyphonic material); musically-weighted pitch analysis; pitched-vs-noise component separation; per-note pitch/timing/volume/formant; ARA integration | Expensive; heavyweight; monophonic vocal shift quality is matched by good PV+formant work for our scope; no noise repair; no RTL language story |
| **iZotope RX 11/12** (Standard $399, Advanced ~$1,199) | Repair Assistant (ML-suggested chain); Dialogue Isolate (ML source separation); combined De-noise+De-reverb with 4-band control; Spectral Recovery (resynthesizes missing HF); Music Rebalance; spectrogram editing | Cost; multi-GB install; offline-first UX (not a live chain); CPU weight; subscription pressure elsewhere in their stack |
| **Antares Auto-Tune Pro 11 / AutoTune 2026** | ~$399 + updates | Real-time Auto Mode (low latency), Graph Mode note editing, key/scale, retune speed, formant, Harmony Player, FX | Pitch-correction *only* — no repair, no mastering, no file management; iLok friction |
| **Adobe Podcast Enhance v2** | Free (cloud) | One-click "studio sound" from anywhere; zero UI learning curve; genuinely good ML | **Cloud-only** (upload your voice = privacy surrender); no control depth (one strength slider); queue/wait; requires account; no offline use |
| **Accentize dxRevive Pro** | ~€399 | Neural speech *restoration* (reintegrates missing spectral content, not just filtering); beats RX Dialogue Isolate and Clarity VX in independent reviews | Neural runtime; cost; restoration (offline) not a live chain; no pitch/formant creative tools |
| **Waves Clarity VX / Vocal Rider / CLA Vocals** | $29–$299 (sales) | VX: real-time neural noise removal. Vocal Rider: automatic level riding. CLA Vocals: curated "one-knob-ish" chain | Nickel-and-dime ecosystem; WUP update fees; each is a single-purpose plugin, not a tool |
| **Krisp / NVIDIA Broadcast** | Freemium / free w/ NVIDIA GPU | Real-time AI noise removal + effects for calls | GPU/runtime heavy; call-oriented (no render/export quality path); NVIDIA Broadcast needs an RTX card |

### 2.2 The positioning this implies

The competitive set splits into three clusters:

1. **Surgical editors** (Melodyne, RX): deep, expensive, heavyweight, offline.
2. **Live/creative tools** (Auto-Tune, Waves): real-time, single-purpose plugins.
3. **One-click enhancers** (Adobe Podcast, Krisp): effortless, shallow, cloud/GPU.

**Micro-Vocal Lab V2's position: the only tool that lives in all three clusters
at once, for free, on any OS, fully offline.** A producer should be able to run
a live low-latency chain while recording *and* render it offline at higher
quality *and* do surgical repair on the result — in one 20-something-MB binary
with no account and no upload. Nobody else occupies that intersection:

- vs **Melodyne**: we won't out-DNA them; we out-*scope* them (repair + dynamics
  + mastering + correction in one tool) and out-price them by €699.
- vs **RX**: we out-*weight* them (22 MB vs multi-GB, 36 ms vs seconds of
  startup) and give real-time preview of the whole chain; they win on
  spectrogram paint tools and ML isolation depth — we close the gap on the
  repair *fundamentals* (noise, reverb, clicks) in 8.4.
- vs **Auto-Tune**: we implement the 90 % case (key/scale snap, retune speed,
  formant preserve, vibrato keep) natively in the chain, in 8.5, at latency
  they'd recognize as professional.
- vs **Adobe Podcast**: we are the local answer. Their one slider becomes our
  *preset* ("Podcast/Broadcast") on a chain the user can then actually edit.
  Their cloud upload is our marketing material.
- vs **Waves**: Vocal Rider and CLA-style color become modules (8.5, 8.6) in a
  chain that also does what none of those single plugins do.

**Reinforcer unique to us**: Arabic-first-class RTL UI (nobody in this market
has it), mm-denominated physical formant model (a Melodyne user thinks in
"formant ±"; a voice clinician thinks in tract length), and GPL-3.0 source.

---

## 3. What must be added, improved, or rejected

### 3.1 Must ADD (competitive table-stakes we lack)

| # | Feature | Mission group | Phase | Competitive answer to |
|---|---|---|---|---|
| A1 | Playhead/sync bug fix | UX | 8.2 | basic credibility |
| A2 | Pitch + formant quality push (transient preservation, full pitch/formant independence, per-band formant, vowel morph) | Quality | 8.3 | Melodyne core |
| A3 | De-noise (profile-learn) | Repair | 8.4 | RX De-noise, Clarity VX |
| A4 | De-reverb (dry/ER/tail) | Repair | 8.4 | RX De-reverb |
| A5 | De-click / de-crackle / plosive | Repair | 8.4 | RX De-click, De-plosive |
| A6 | Harmonics/saturation (tube/tape/transistor) | Color | 8.5 | CLA Vocals, analog color |
| A7 | Auto-Tune pitch correction (key/scale/retune/strength, vibrato + formant preserve, pitch-track visual) | Creative | 8.5 | Auto-Tune Pro |
| A8 | Vocal rider / LUFS leveling | Dynamics | 8.5 | Waves Vocal Rider |
| A9 | Mastering chain (input gain, multiband comp, limiter, dither, full LUFS metering) | Master | 8.6 | Ozone-lite |
| A10 | 4-band parametric EQ + curve graph | Tone | 8.7 | every DAW |
| A11 | Compressor + GR meter; de-esser panel w/ freq highlight | Dynamics/Tone | 8.7 | every DAW |
| A12 | Multi-track (load, chain-per-track, mix) | Workflow | 8.8 | DAW-lite |
| A13 | Batch processing (folder → same chain → export, progress) | Workflow | 8.8 | RX batch |
| A14 | Presets (built-in 7 personas + user, JSON import/export) | Workflow | 8.9 | all of them |
| A15 | A/B compare + copy + visual diff | Workflow | 8.9 | plugin standard |
| A16 | Undo/redo (full param history, Ctrl+Z/Y, panel) | Workflow | 8.9 | basic credibility |
| A17 | Project files (save/load/auto-save/crash recovery) | Workflow | 8.9 | DAW standard |
| A18 | FLAC + OGG export, batch naming templates | Workflow | 8.9/8.11 | distribution formats |
| A19 | UI pass: panel tabs, spectrogram view, markers, region coloring, accessibility, shortcuts panel | UX | 8.10 | RX/Melodyne UX |
| A20 | Test/scale-up: 400+ unit, 50+ integration, 50-sample golden corpus, 10k fuzz, 8 h soak | Quality | 8.11 | professional QA bar |

### 3.2 Must IMPROVE (exists, below bar)

| # | Current | Target | Phase |
|---|---|---|---|
| I1 | PV pitch shift, artifact-controlled | zero-audible-artifact ±12 st on sung/spoken/vibrato/transient material; transient-preserving offline path; **< 10 ms** preview latency (from 10.7) | 8.3 |
| I2 | Formant warp coupled to pitch path | fully independent axes + formant lock + per-band curves + A/E/I/O/U morph | 8.3 |
| I3 | Air/breath one macro knob | separate **air** (HF shimmer) vs **breath** (noise) controls; de-esser freq select; plosive + mouth-noise removal; keep 0.1 dB precision | 8.3/8.4 |
| I4 | 40-band RTA | + peak-hold decay control, scrollable spectrogram, frequency cursor readout, higher resolution | 8.10 |
| I5 | Waveform view | sample zoom (have) + time-selection readout, loop region, markers, region coloring by classifier (voiced/breath/silence) | 8.10 |
| I6 | Keyboard navigation | full keyboard map + shortcuts panel + high-contrast mode + resizable panels | 8.10 |
| I7 | 5-OS CI | keep 10/10 green every phase; add per-phase size/RSS gates so regressions block merge | every |

### 3.3 REJECTED for v2.0.0 (with reasons — per the autonomy directive)

| Rejected | Reasoning |
|---|---|
| **Bundled neural models** (DDSP/RAVE/DiffSVC/DeepFilterNet-class) | 2026 SOTA neural enhancement models that beat classical DSP start at ~5–50 MB and require an inference runtime (tract/ONNX) with its own size, cold-start and cross-platform-verification cost. Bundling would break the < 50 MB / sub-second / RAM-light identity that *is* our moat, for quality gains concentrated in one module (noise/reverb). Classical DSP closes 80 % of that gap at 0 MB. **The V2 engine architecture reserves a `ModelSlot`** (versioned, local-file, user-installed, never auto-downloaded) so a post-2.0 "Neural Pack" is additive, not a rewrite. |
| **DNA-style polyphonic note editing** | Research-scale problem; monophonic voice is the product scope (see §1.3). |
| **Real-time *streaming* de-reverb / restoration during live monitoring at < 10 ms** | Honest physics: dereverberation needs lookahead; we ship de-reverb as an offline + preview-latency-tolerant module, not as a zero-latency monitor effect. |
| **Ogg Vorbis *encode* via FFI libvorbis** | No maintained pure-Rust Vorbis encoder exists (lewton is decode-only); bundling C libvorbis works (LAME precedent) but is queued *behind* FLAC (pure-Rust `flacenc`) — Vorbis lands only if 8.9's budget check permits; otherwise documented as 2.1. |
| **Preset "marketplace"** | Stays a local folder + JSON import/export. No cloud, per §1.3. ("Marketplace" in the brief is satisfied by import/export; no server.) |

---

## 4. DSP technology strategy

### 4.1 Classical-first, neural-ready

V2 ships **deterministic, inspectable, CPU-light classical DSP**, module by
module, each held to the artifact bar by benchmark (§6). This is not
cost-cutting: on monophonic voice in 2026, well-implemented classical methods
sit within the perceptual gap of neural ones for *every module we ship*
(profile-learned spectral de-noise, LP-based de-reverb, AR-interpolation
de-click, waveshaper saturation, scale-snapping correction) — while neural wins
concentrate in source *separation* and *resynthesis*, which we defer via the
ModelSlot. Every algorithm choice below is benchmarked in its phase and the
numbers land in `docs/V2_BENCHMARKS.md`; if a classical module misses its bar,
the phase report says so and the roadmap re-prioritizes.

### 4.2 Algorithm selection per module (with rationale)

**Pitch (8.3).** Keep the identity-locked PV as the real-time spine. Add: (a)
transient *preservation* — the existing 1.3 ms transient detector currently
gates; upgrade to frame-level time-domain splice-through (PV bypass for
transient frames, Laroche-style), the standard fix for PV smearing; (b) an
offline Render-profile path with longer windows + stricter phase locking
("phase-vocoder-done-right" class refinements: peak-based region growing,
identity-lock audit); (c) full **decoupling from formants** — pitch shifts
resynthesize on the *original* spectral envelope (we already compute a
cepstral true envelope for the formant warper — reuse it as the resynthesis
envelope), making "pitch without formant" and "formant without pitch" the same
machinery with two different ratio knobs.

**Formants (8.3).** The true-envelope + Bark-warp engine generalizes: per-band
ratios (the envelope is already per-bin — expose 3–5 bands), and vowel morphing
= morphing the current envelope toward a stored target-envelope set
(a/ɛ/i/ɔ/u templates in tract-length-normalized formant space, F1×F2×F3
warped by the user's tract mm so they stay *physically coherent*).

**De-noise (8.4).** Noise-profile learning (user selects ≤ 1 s of noise, or
minimum-statistics auto-profile) → per-bin a-priori SNR estimate with
decision-directed smoothing (Ephraim–Malah-class Wiener gain, log-domain),
cepstral-smoothed gain floor to kill musical noise, classifier-gated so voiced
bins get gentler treatment. Zero learned weights; profile is ~200 floats.

**De-reverb (8.4).** Honest scope: spectral late-reverb suppression keyed on
envelope-decay estimation (reverberation tail = slower envelope decay than
direct sound) with dry/wet + tail-length controls; early reflections largely
preserved (they *are* the room timbre users complain least about). Not WPE;
documented as such.

**De-click / de-plosive (8.4).** Click detection via high-passed
prediction-error energy (AR model already needed for other modules); repair by
short AR interpolation across the damaged span. Plosives = the existing
transient detector + low-band energy signature → attenuate + spectral-fill.
Mouth noise = short unvoiced non-sibilant transients → attenuate.

**Saturation (8.5).** Three deterministic waveshapers — tube (asymmetric
2nd/3rd-order polynomial + slight expansion), tape (soft-clip knee +
hysteresis one-pole), transistor (harder clip knee) — each with drive/mix/trim,
all inside the existing 4× oversampler (the limiter already proves the
infrastructure). Anti-aliased by construction.

**Pitch correction (8.5).** The YIN track already runs per frame. Pipeline:
median-filter F0 → snap to selected key/scale grid (cents tolerance for
"flex" behavior) → retune-speed one-pole toward target (0–400 ms) →
vibrato preservation (separate the ±50-cent 4–8 Hz modulation component and
re-add it post-correction) → resynthesize via the *existing* pitch path driven
per-frame by (measured F0 → desired F0) ratio. Formant preservation comes free
from 8.3's envelope decoupling. The visual pitch track with target notes is a
new waveform-overlay panel.

**Vocal rider (8.5).** Short-term LUFS (K-weighted, BS.1770) target with
attack/release ballistics on a smoothed gain curve; "preserve dynamics" =
gain moves limited to a rate ceiling; output trim keeps peaks under control
before the master limiter.

**Mastering (8.6).** Input gain → 3-band Linkwitz-Riley crossover compressor
(per-band threshold/ratio/attack/release, master GR meter) → true-peak limiter
(extend the existing 4× oversampled guard to a full lookahead brickwall with
character control) → dither (TPDF, 16/24-bit targets) → LUFS metering
(momentary/short-term/integrated + true peak), all per BS.1770-4.

**EQ (8.7).** Four RBJ biquads (HP/low-shelf/peaking/high-shelf parametric) +
curve graph (FFT of the cascade impulse, drawn in the panel), A/B per band,
all-UI-real.

**Engine architecture.** `VocalParams` (3 floats) grows into a versioned,
`Copy`, per-module param block (one struct, `#[repr(C)]`-stable layout) pushed
through the existing lock-free slot — zero alloc/zero lock in the callback is
an invariant that ships with tests. Modules form an ordered chain with
per-module bypass; **bypass of every module must be bit-exact** (generalized
invariant #1). Chain order is fixed in 8.x as modules land:
`repair → de-esser → EQ → pitch/formant → correction → air → saturation →
dynamics → rider → master`. Preview and Render stay one algorithm, two
profiles. 192 kHz / 32-float end-to-end stays. Offline oversampling (2×/4×)
is a Render-profile option. Bit-exact reproducibility: golden-sample CI
regression, the pattern already proven.

### 4.3 Real-time performance contract (unchanged, re-asserted)

- < 10 ms preview algorithmic latency (8.3 must shave 10.7 → < 10: shorter
  resampler tail in Preview profile; measured by the existing test).
- Zero allocations, zero locks, zero blocking in the audio callback (audited
  per module; the preview path already proves it).
- Render strictly faster than realtime (currently 22.7×; modules must keep the
  product ≥ 5× on a 2020-class CPU at 48 kHz — CI perf smoke).
- Binary < 30 MB, cold start < 500 ms, RSS < 100 MB typical / < 300 MB heavy
  (multi-track). CI gates enforce size; perf evidence lands per phase.

---

## 5. Roadmap (mission phases, acceptance criteria)

Each phase: implement → test → benchmark → commit+push → phase report → STOP.

| Phase | Scope | Acceptance criteria (measurable) |
|---|---|---|
| **8.1** | this strategy | doc merged; baseline re-verified (done: 217/217) |
| **8.2** | playhead sync bug | root-cause writeup; sync test asserting \|visual − audible\| ≤ 1 UI frame; ONDEVICE row for manual check; no regression in 217 |
| **8.3** | pitch + formant to the bar | ±12 st on the §6 corpus: F0 error ≤ 10 cents sustained, THD+N ≤ −40 dB on pure tone, no audible transient smear on plosive material (listening panel ≥ "clean" on 5-pt scale); formant independence: pitch-only shift changes F0 ≤ 1 % with F1/F2 Δ ≤ 3 %; vowel morph A↔U perceived correctly ≥ 90 % in panel; preview latency < 10 ms measured; all golden tests green |
| **8.4** | de-noise, de-reverb, de-click | de-noise: ≥ +10 dB segSNR on fan/hum corpus with ≤ −40 dB speech distortion (LSD); de-reverb: dry/wet monotonic, tail control effective on RT60 0.4–2 s corpus, no pumping; de-click: ≥ 95 % detection on synthesized-click corpus with ≤ 1 % false positives, inaudible repair; every module bit-exact on bypass |
| **8.5** | harmonics, correction, rider | correction: snaps to scale within tolerance, retune 0–400 ms, vibrato preserved (4–8 Hz modulation retained ≥ 80 %); saturation: THD spectrum matches the three characters' targets, aliasing ≤ −80 dB oversampled; rider: short-term LUFS within ±1 LU of target on 30 s mixed-level corpus; pitch-track visual live |
| **8.6** | mastering + LUFS | LUFS meter validated against BS.1770-4 test vectors (built-in); limiter true-peak ≤ −1 dBTP at max drive, no pumping on 10 ms burst corpus; integrated-LUFS target render ±0.5 LU end-to-end |
| **8.7** | EQ + dynamics + de-esser panels | EQ curve matches analytically computed response ≤ 0.5 dB at 30 points; compressor GR meter within ±0.5 dB of analytic envelope; de-esser band-limit verified by spectrum diff; panels real (no fake UI — pixel-verified) |
| **8.8** | multi-track + batch | ≥ 4 tracks mixed in preview + export within latency/CPU budget; batch of 100 files processes unattended, progress-reported, resumable, zero crashes (fuzz-grade inputs) |
| **8.9** | presets, A/B, undo, projects, FLAC/OGG | 7 built-in presets audible and distinct (panel-verified); A/B copy + diff correct on 20 random param sets; undo/redo 200-step history exact; project round-trip (open→save→open) bit-identical render; crash-recovery journal survives kill -9; FLAC encode bit-verified round-trip vs reference decode |
| **8.10** | UI/UX pass | spectrogram + markers + region coloring + shortcuts panel + high-contrast + resizable panels; RTL still pixel-verified; VLM critique ≥ 8/10 held; keyboard-only walkthrough passes |
| **8.11** | verification + scale-up | 400+ unit / 50+ integration tests; 10,000-file fuzz zero panics; 8 h soak zero crashes, RSS flat; 50-sample golden corpus in CI (two-tier tolerance); all 5 OS legs green; size/RSS gates enforced |
| **8.12** | release | v2.0.0 tag, all 12 phase reports linked, benchmarks doc complete, GitHub Release with permanent binaries, README/manual (EN+AR) refreshed |

---

## 6. Benchmarking methodology (`docs/V2_BENCHMARKS.md`, built incrementally)

**Corpus (built in 8.3, extended per phase).** 50+ real-world samples: sung
phrases (male/female, vibrato-heavy), spoken phrases (EN + AR), plosive-heavy
readings, breathy ASMR-style, plus deterministic synthesized anchors (sweeps,
harmonic stacks, impulse trains) — the existing `testdata/` generators extended.
Licensing: synthesized + self-recorded + CC0 clips only (repo-safe); corpus
generation is scripted and reproducible (`scripts/`), sizes kept small.

**Objective metrics per module** (all computed by `scripts/` tooling, results
committed as machine-readable evidence):

| Module | Metrics |
|---|---|
| Pitch | F0 error (cents, YIN on in/out), THD+N on pure tones, spectral-flatness of residual (artifact energy), phase-coherence proxy (inter-frame phase deviation), duration bit-exactness |
| Formant | F1–F3 error (LPC analysis on in/out), envelope log-spectral distance |
| De-noise | segSNR improvement, speech-domain LSD, musical-noise count (isolated bin activations) |
| De-reverb | envelope-decay fit (RT60 proxy pre/post), direct/tail energy ratio |
| De-click | detection TP/FP rates, repair-span distortion |
| Saturation | harmonic profile vs target, alias level (oversampled residual) |
| Correction | post-correction F0 − grid error, vibrato retention, latency |
| Rider/Master | short-term LUFS error vs target, true-peak margin, crest-factor retention |
| System | preview latency (existing test), render ×realtime, RSS, cold start, binary size |

**Golden-sample CI regression** (existing pattern): every module's render on
the corpus anchors is bit-exact on the fixture platform, ≤ 1e-4 (−80 dBFS)
elsewhere — a single `cargo test` gate per merge.

**Competitor comparison (honesty rules).** Same corpus through reference
tools where legally runnable (Melodyne 30-day demo, RX trial, Auto-Tune demo)
on an on-device session; metrics computed identically on their outputs.
Where this sandbox cannot install them (no GUI/licenses), the benchmark doc
records (a) our absolute numbers, (b) the on-device methodology for the
comparison run, and (c) vendor-published specs as *claims*, clearly labeled —
never presenting vendor marketing as measured results. Subjective A/B
listening tests: MUSHRA-lite protocol (5+ raters, defined anchors, randomized
pairs, scores + notes recorded in the doc). Every phase report carries its
benchmark table; failures are reported, not hidden — the Phase 3/6/7 honest
gaps tradition continues.

---

## 7. Risk register (top 8)

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Param-struct growth breaks the real-time `Copy` contract | medium | high | versioned `#[repr(C)]` block designed once in 8.3, extended by-append; slot stays lock-free; callback audit per module |
| Quality bar missed on a module (artifact metrics fail) | medium | high | benchmark gate is the phase exit criterion; strategy §4.1 allows re-scoping with a written rationale (the autonomy directive honored *transparently*) |
| UI density collapses usability as panels multiply | high | medium | tabbed module groups (Repair/Tone/Dynamics/Master) in the existing rack language; 8.10 includes a UX pass + VLM critique loop (the 7.2 process, which lifted 4/10→8/10) |
| Size/RAM creep from new deps (serde, flacenc, …) | medium | medium | CI size gate: binary ≤ 30 MB, RSS budgets; cargo-bloat diff per phase; every dep justified in the phase report |
| Multi-track blows the RAM budget | medium | medium | disk-backed tracks (the recording path already streams; import stays RAM-resident only under a cap) |
| Playhead sync root cause worse than expected | low | medium | 8.2 is scoped to fix + regression-test, not to also refactor transport; refactor is a fallback with its own report |
| Regression in the 217-test invariant spine while refactoring | medium | high | every phase keeps the full suite green *plus* its own new tests; no "fix later" merges |
| Windows/macOS hardware behaviors (device quirks) resurface | medium | medium | the 7.5 pattern: classify, ladder, diagnose, document; on-device kit remains the truth source |

---

## 8. What success looks like (v2.0.0 quality bar)

A real producer/podcaster/singer, on any of 5 OS targets, on a laptop from the
last 6 years:

1. Opens in under half a second to a professional studio UI (held to the 8/10
   critique bar or better).
2. Records any mic (device ladder + privacy diagnostics keep the 7.5 record).
3. Runs a full chain — repair (noise/reverb/clicks), tone (EQ, de-ess,
   saturation), pitch (shift + correction + formants + vowel), dynamics
   (compressor, rider), master (multiband, limiter, dither, LUFS) — with every
   parameter real, A/B-able, undoable, preset-able.
4. Previews it live under 10 ms and exports it at higher quality, hearing
   exactly what they previewed.
5. Exports WAV/MP3/FLAC (OGG if budget permits), batch or per-file, from a
   saved project with crash recovery.
6. Never sees a crash (fuzz + soak evidence), never uploads a byte (no network
   code path at all), never pays a cent, and — for Arabic-speaking users —
   works natively RTL, which none of the incumbents offer.

That is a product people choose over a paid alternative. That is V2.

**Next: Phase 8.2 — fix the playhead/audio sync bug.**
