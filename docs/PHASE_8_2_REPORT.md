# Phase 8.2 Report — Playhead / Audio Synchronization Fix

**User-reported bug:** "When playing audio, the sound does not match the visual playhead."
**Deliverable:** the playhead now reports the *audible* position — anchored to the
device clock, latency-compensated, interpolated between device callbacks — plus a
13-test regression battery with a 5 ms end-to-end acceptance test.
**Baseline preserved:** `cargo test --workspace` on this tree: **230 passed /
0 failed / 5 ignored** (217 pre-existing + 13 new; the 5 hardware-gated ignores
unchanged), `cargo fmt --check` clean, `cargo clippy -D warnings` clean,
virtual-device smokes green (`MVL_VIRTUAL_AUDIO=1`).

---

## 1. Root causes (audited against the code, all four hypotheses from the plan)

The UI playhead is driven by `ui_tick` (33 ms Slint timer) reading
`PreviewPlayer::position_seconds()` (`mvl-app/preview.rs`); the standalone
`mvl_io::player::Player` used by `selftest` had a mirror implementation. Both
computed the position as **`base + fed − consumed`** (in samples). With

* `base` — transport position (samples) at the last play/seek/stop,
* `fed` — samples the feeder pushed into the ring since the bump,
* `consumed` — samples the device callback popped since the bump,

`fed − consumed` is exactly the **ring occupancy**. Both feeders keep the ring
topped up (the preview feeder fills until less than one 2048-frame block is
free; the mvl-io feeder fills until free space drops below one 4096-frame
chunk of a 0.5 s ring), so steady-state occupancy is ~0.4–0.5 s of audio.

1. **Wrong cursor (dominant, confirmed).** `base + fed − consumed` is the
   *feeder's write cursor* — audio that is queued but has never been handed to
   the device. The playhead led the audible audio by the entire ring fill
   (~0.5 s), jittering up/down by callback-sized steps every time the feeder
   topped the ring up (visible jumping). This alone explains the report.
2. **Device latency not subtracted (confirmed).** Even the *handed* cursor
   (`base + consumed`) leads what is audible: the OS/device pipeline holds
   another buffer between the callback and the DAC (10–50 ms typical:
   WASAPI shared padding, CoreAudio output latency, ALSA `snd_pcm_delay`).
3. **Sample-rate mismatch (already structurally handled; verified).** Sources
   are resampled once to the device rate at construction
   (`mvl_io::channels::adapt`), and the shared state carries the *adapted*
   rate; the playhead therefore always ran on the device rate. The gap was
   that nothing *tested* the seconds-mapping after adaptation — added
   (`playhead_uses_device_rate_after_resample`).
4. **Clock drift (confirmed as a non-issue once anchored).** The old code had
   no wall-clock extrapolation at all, so drift was masked by bug 1. The fix
   re-anchors on every device callback, so device-vs-system drift cannot
   accumulate (residual error is bounded by one callback period).

## 2. The fix (`crates/mvl-io/src/playhead.rs`, new)

### 2.1 Anchor in the audio callback, on the device's clock

Each callback that hands audio to the device publishes a **playhead anchor**
into a seqlock-protected lock-free slot:

* `first_frame` — frame position of the first sample popped in this callback
  (`(base + consumed_at_entry) / channels`);
* `at` — `Instant::now()` captured in the callback (the process monotonic
  clock the UI also reads);
* `latency` — the measured callback→audible delay, taken from cpal's output
  timestamp pair: `playback − callback` (`OutputStreamTimestamp`), which the
  backends populate from ALSA `snd_pcm_delay`, WASAPI padding and CoreAudio
  latency. When a backend provides no prediction, the measured callback
  period / 2 is used as the fallback.

`StreamInstant` values are opaque host-clock readings that cannot be compared
across threads; pairing the callback's `Instant::now()` with the host's
*relative* `playback − callback` duration maps the anchor into the process
monotonic clock without losing the device-clock semantics.

### 2.2 Extrapolation on the UI side (the interpolation fix)

`audible_position_seconds()` computes, per UI tick:

```text
audible(T) = first_frame + (T − callback − latency) · rate
```

* exact between callbacks (the device drains its buffer at a constant rate),
  so the 30 fps timer displays a smoothly interpolated position instead of
  callback-quantized jumps;
* re-anchored on every callback — device/system clock drift cannot accumulate;
* clamped to `[0, handed cursor]` — the playhead never leads audio that has
  not been handed to the device, so an underrun freezes the playhead instead
  of running it ahead of silence;
* clamped to the track length.

### 2.3 Accounting corrections

* **Flushed ≠ handed.** On seek/stop the sink discards buffered ring content;
  those samples never reached the device and are no longer counted as
  consumed, so the handed cursor (the playhead's floor) stays honest across
  seeks.
* **Counter/anchor reset ordering.** `bump_generation` now publishes the new
  base and zeroed counters *before* the generation increment (release /
  acquire pairing through `generation`), and freezes the anchor at the new
  base — after a seek the playhead parks at the seek target until the first
  real callback re-anchors, so there is no jump-back-then-leap.
* **Pause freezes at the resume point** (the first un-popped frame), which is
  what the ring preserves and playback resumes from.

### 2.4 Real-time safety

The anchor write is four atomic stores plus one `Instant::now()` — no locks,
no allocation, no blocking in the audio callback. The reader is a bounded
seqlock read (32 attempts, spin then yield) that can never spin unbounded
against an RT-priority writer; on the vanishingly unlikely retry exhaustion
it degrades to the handed cursor (upper bound, ≤ one device buffer ahead)
for a single tick. The process epoch for nanosecond timestamps is pinned in
`PlayheadAnchor::new()`, guaranteeing every anchor `Instant` maps to a
monotonic non-negative reading.

### 2.5 Applied in both players

`mvl_io::player::Player` and `mvl_app::preview::PreviewPlayer` share
`audible_position_seconds()`; both sinks publish anchors in all six device
sample-format paths (F32 direct + I16/I32/U16/U32/U8 through the shim). The
`fed` counter remains published (diagnostics) but no longer feeds the
position. UI code (`controller.rs`) needed no changes beyond the existing
30 fps tick — the correctness moved into the position source. A small
`Controller::park_playhead()` was added so the screenshot path publishes
line + timecode + visibility together (previously it set the line only,
leaving the timecode stale in evidence renders).

## 3. Verification

### 3.1 New tests (13)

| Test | Asserts |
|---|---|
| `playhead::extrapolation_matches_device_time` | anchor + extrapolation reproduces device time within 2 ms |
| `playhead::latency_pulls_playhead_behind_handed_cursor` | reported ≤ audible bound during the latency window |
| `playhead::playhead_never_leads_handed_cursor` | 10 s of wall time with no callbacks cannot push past the handed cursor (underrun freeze) |
| `playhead::frozen_when_not_playing` | pause freezes at the resume point |
| `playhead::reset_frozen_stops_extrapolation` | seek parks at the target until the next callback |
| `playhead::clamps_to_track_length` | never reports past the end |
| `playhead::channels_divide_samples_into_frames` | interleaved-sample counters map to frame positions for stereo |
| `playhead::seqlock_reader_never_torn_under_write_pressure` | 20 k reads against a ~10 kHz writer, no torn/garbage values |
| `playhead::host_output_latency_extracts_positive_delta` | `playback − callback` extraction incl. equal/earlier instants → `None` |
| `player::flushed_audio_not_counted_as_handed` | flushed ring content leaves the handed cursor intact |
| `player::sink_publishes_playhead_anchor_at_correct_frame` | anchor frame positions per callback, no update on underrun |
| `player::e2e_playhead_matches_audible_audio_within_5ms` | **the acceptance test**: real feeder thread + ring + sink pumped by a simulated paced device (10 ms blocks, 21 ms host latency); reported vs audible ≤ 5 ms at every probe (≪ 1 UI frame = 33 ms) |
| `player::playhead_uses_device_rate_after_resample` | after 44.1 kHz → 48 kHz adaptation one second of handed audio advances the playhead exactly one second |

### 3.2 Suite health

* `cargo test --workspace`: **230 passed / 0 failed / 5 ignored** (baseline 217
  reproduced by Phase 8.1 on this tree; +13 new, zero regressions).
* `cargo fmt --all -- --check`: PASS. `cargo clippy --workspace --all-targets
  -D warnings`: PASS.
* `MVL_VIRTUAL_AUDIO=1` virtual-device smokes (real cpal/ALSA null device
  through the new anchor code): `virtual_player_smoke`,
  `virtual_preview_smoke`, `virtual_recorder_smoke` — green.

### 3.3 Evidence renders

`docs/phase8-evidence/` — headless studio renders with the playhead parked
through the same three properties the live seek path publishes; line and
timecode agree in every render:

* `playhead-at-0.30s.png` — playhead at 300 ms, timecode `00:00:00.300`;
* `playhead-at-0.75s.png` — playhead at 750 ms, timecode `00:00:00.750`;
* `playhead-at-1.20s-ar.png` — 1.20 s in Arabic/RTL (mirrored layout intact).

Note honestly: the container has no sound card, so *perceived* sync on real
hardware is verified by the ONDEVICE protocol (§4) — the machinery is proven
here by the e2e test, which replaces only the OS device with a paced
simulator while running the real feeder, ring, sink and anchor code.

## 4. On-device manual check (ONDEVICE.md row 5 sharpened)

Play a 44.1 kHz file and a 48 kHz file; the playhead must stay within ~1 UI
frame (33 ms) of what you hear — no leading ahead of the audio, no jumping;
pause must freeze exactly where audio stops; seek must land the playhead on
the target with no leap; the same after a language switch to عربي.

## 5. Honest gaps

* Hosts that report no playback instant (e.g. PulseAudio ALSA plugin with
  zero htstamp) fall back to half the measured callback period — typically
  within a few ms of the true latency, but not host-guaranteed.
* After a seek there is a ≤ one-callback transient in which the sink may
  briefly report a pre-bump handed position (bounded by one device buffer,
  self-corrects at the next callback).
* The e2e test simulates the OS device (real-time paced pops); it cannot
  catch host-specific timestamp bugs — that is what the on-device protocol
  and the platform CI legs are for.
* Known aesthetic (pre-existing, out of scope): the mvl-io feeder and preview
  feeder still keep ~0.5 s of ring buffered; harmless since the playhead no
  longer reads the write cursor, and it keeps seeks responsive.

## 6. Exit state

Fix + tests + docs committed and pushed; WORKLOG and ONDEVICE updated. The
playhead reads the audible position on all platforms where cpal exposes the
playback instant (Windows WASAPI, macOS CoreAudio, Linux ALSA), with a
measured fallback elsewhere. Waiting for "continue" → **Phase 8.3 (EQ panel,
4-band parametric)** per V2_STRATEGY §5.
