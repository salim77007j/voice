# On-device verification — Phase 5 protocol

CI proves the code paths on five OS configurations (including a virtual
ALSA device for the audio stack), and the container proves the offline
engine end to end. Neither can prove **perception**: that the window looks
right on a real GPU display, that dialogs open, that a take recorded from
a real microphone *sounds* natural through the engine. That is this
document — a ~10-minute human protocol, run on a real desktop (Windows,
macOS or Linux), producing the evidence the phase report cites.

## 1. Automated part — `selftest` (2 minutes)

```sh
cargo build --release --bin micro-vocal-lab
./target/release/micro-vocal-lab selftest            # 5 s take
./target/release/micro-vocal-lab selftest --seconds 12 evidence/
```

The command records from the **default input device** (speak — a loud,
breathy phrase is ideal material for the air/breath engine), plays the take
back through the **live engine** (listen: pitch +3 st, air −30 %,
tract 140 mm), renders the offline export in both codecs, and renders the
real UI in English and Arabic/RTL. Everything lands in the output
directory:

| File | What it is evidence of |
|---|---|
| `report.txt` | PASS/FAIL/SKIP table + verdict (exit code 0 = no failures) |
| `recording.wav` | real capture: negotiated rate/channels, disk streaming, no drops |
| `exported.wav` / `exported.mp3` | offline Render-profile engine + both codecs |
| `screenshot-en.png` / `screenshot-ar.png` | the real UI component tree, EN and AR/RTL |

Notes the selftest writes honestly: a device that caps below the 192 kHz
studio target is a **PASS with a fallback note** (requirement R4), a
nearly-silent input is a PASS with a "mic muted?" warning, a machine with
no output device SKIPs only the live-preview step.

## 2. Manual part — the human checklist

Run the app (`micro-vocal-lab run` or `cargo run --release -p mvl-app`),
then verify by hand. Tick each row; anything that fails becomes a Phase 6
gap, not a silent pass.

| # | Check | Pass criteria |
|---|---|---|
| 1 | Window opens | real GPU renderer, dark Precision Studio theme, < 1 s cold start |
| 2 | Import via dialog | native file dialog opens (not a stub), WAV **and** MP3 load, waveform + duration + rate appear in the status bar |
| 3 | Record | *New Recording* arms red; a take records to disk; status shows the negotiated rate — if the device caps at 96/48 kHz the fallback notice is shown |
| 4 | Preview with live sliders | during playback move pitch — audibly shifts within a hop (<3 ms, no clicks); air −100 % visibly ducks breaths on the meter; formant changes timbre without chipmunking |
| 5 | Waveform | wheel-zoom reaches *sample level* (stem plot); *fit* restores; playhead tracks audio; click seeks |
| 5b | **Playhead sync (Phase 8.2 protocol)** | play a **44.1 kHz** file and a **48 kHz** file (one import of each): during playback the line stays within ~1 UI frame (33 ms) of what you hear — never running ahead of the sound, no visible stepping/jumping; **pause** freezes exactly where audio stops and resume continues there; **click-seek** lands the line on the target with no leap; verdict unchanged after switching to عربي (RTL) |
| 6 | Export via dialog | native save dialog; WAV export of a 192 kHz take stays full-rate; MP3 of a 48 kHz import round-trips |
| 7 | عربي + RTL | language switch is instant; toolbar/status mirror; side panel moves left; sliders mirror their drag direction; digits stay Western |
| 8 | Keyboard-only | unplug the mouse: Tab reaches every control (2 px teal focus ring), Space play/pause, arrows step 1 cent / 0.1 dB / 1 mm, Shift = 10×, Home/double-click resets |
| 9 | Listening verdict | the honest human call: does a +3 st shift still sound like the same voice? Note anything metallic/phasey for the Phase 6 report |
| 10 | Attach | the `selftest` output directory + a screenshot of the loaded window + your listening notes |

## 3. Troubleshooting

- **macOS, unsigned build**: Gatekeeper blocks first launch —
  `xattr -cr micro-vocal-lab` or right-click → Open. Microphone access
  must be granted to the Terminal app in System Settings → Privacy.
- **Linux dialogs**: file dialogs go through the XDG portal — install
  `xdg-desktop-portal-gtk` (or `-kde`) if no dialog appears.
- **Bluetooth headsets** negotiate low rates (16/24/48 kHz) — the recorder
  fallback note is expected behaviour, not a bug; use a wired/built-in
  mic for 192 kHz verification.
- **Linux audio stack**: the selftest prints the cpal host (`ALSA`,
  `CoreAudio`, `Wasapi`); if capture fails under a PipeWire system, check
  `pw-loopback` / default device selection first.

## 4. Where the results go

`docs/PHASE_6_REPORT.md` (the validation report) cites this run: the
selftest `report.txt`, the screenshots, and the checklist verdicts — the
honest-gaps section is fed directly from rows that failed or were skipped.
