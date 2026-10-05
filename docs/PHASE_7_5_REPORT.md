# Phase 7.5 Report — v1.1.1 (Windows mic Access Denied + import RefCell panic)

**Bugs fixed:** 2 P0s reported by real Windows users on v1.1.0
**Tests:** 196 → **217** (+21), all green locally · clippy `-D warnings` clean · fmt clean
**Root-cause proof:** BUG 2's regression tests reproduce the user's exact panic
(`controller.rs:977:31: RefCell already borrowed`) on the pre-fix code, and pass on the fix.

---

## BUG 1 — Windows microphone open failure: `Access is denied. (os error -2147024891)`

### Root cause analysis (all five hypotheses from the report investigated)

| Hypothesis | Verdict | Evidence |
|---|---|---|
| **Privacy permission gate** | **Primary cause on stock Windows 10/11.** WASAPI returns `E_ACCESSDENIED` (0x80070005) from `IAudioClient::Initialize` when the CapabilityAccessManager has not granted capture to desktop apps. No format change can fix this — the gate is per-app, not per-format. | The user's error text matches `E_ACCESSDENIED` exactly; enumeration succeeded (the device was found), so the app could *see* the mic but was *denied* the stream. |
| **Unsupported requested format** | **Real, secondary.** v1.1.0's `start_on` computed exactly one config and made exactly one open attempt; if that config failed the whole *device* was abandoned, even though another format/rate/channel count on the *same* device would have opened. | Code: old `Recorder::start_on` called `negotiate()` → single `spawn()`; failure skipped to the next device. |
| **Exclusive vs shared mode** | **Ruled out by design.** cpal 0.18 initializes WASAPI streams in **shared mode only** (`AUDCLNT_SHAREMODE_SHARED`); the app never requests exclusive mode. | cpal 0.18.2 WASAPI backend has no exclusive-mode API surface. |
| **Wrong device class (communications/loopback/disabled)** | **Mostly ruled out.** cpal enumerates only *active* capture endpoints (`DEVICE_STATE_ACTIVE`), so disabled/disconnected devices never appear; communications devices are ordinary capture endpoints and open normally. Loopback capture is not exposed by cpal on WASAPI at all. | cpal WASAPI `Device` impl. |
| **Missing capability/manifest declaration** | **Ruled out.** The microphone privacy gate for desktop (non-UWP) apps keys on the exe being an *unpackaged desktop app* — which is exactly what we ship (plain `micro-vocal-lab.exe`, no embedded manifest needed; the "Let desktop apps access" toggle applies). No additional manifest is required for WASAPI capture. | Windows docs on CapabilityAccessManager + the fact that v1.0.0 users on other machines recorded successfully with the identical binary. |

### The fix — three layers in `mvl-io`

**1. Format-negotiation ladder (`recorder.rs::negotiation_ladder`, pure + unit-tested).**
`start_on` now walks an ordered ladder and performs an *actual stream open* for each rung
until one plays — negotiation means opening, not just choosing:

```
0. classic negotiate() pick (v1.1.0 studio rules)
1. requested rate × requested channels      (formats in preference order F32,I16,I32,U16,U32,U8)
2. requested rate × mono
3. 48 kHz × requested channels
4. 48 kHz × mono
5. 44.1 kHz × requested channels
6. 44.1 kHz × mono
7. the device's own default config   ← on WASAPI this is the shared-mode mix format,
                                        which the engine always accepts
8. every reported range's max + min representatives (8 kHz capture beats no capture)
```

Deduplicated, capped at 24 rungs. **Every failed rung is logged** with its exact config and
reason (`mvl-io: '<device>' rung 2/9 (48000 Hz, 1 ch, F32) failed: …`), and a successful
fallback announces which rung opened. 9 unit tests cover ordering, mono fallback,
device-default trust, dedup, unreported-combo exclusion, and the cap.

**2. Access-denied classification (`consent.rs::is_access_denied`).**
Detects `E_ACCESSDENIED` from the error text in every form cpal prints it
("Access is denied", `os error -2147024891`, `0x80070005`) — unit-tested against the
user's verbatim crash string. When a rung fails with access denied, the ladder **stops
hammering that device** (every format would fail identically) and moves on; when *every
device on the machine* failed with access denied, the final error is no longer
"tried N devices" but the privacy message below.

**3. Windows privacy diagnostic (`consent.rs::microphone_consent`).**
Reads the registry keys behind the Settings page —
`HKCU\…\CapabilityAccessManager\ConsentStore\microphone` (per-user) and the `HKLM` twin
(machine policy) — via `reg.exe query` (zero new dependencies, no unsafe code, compiles
away on other platforms). The error message then states which toggle is off:

```
Windows denied access to the microphone (tried 3 input device(s)).
This is the OS privacy gate, not a device or format problem — every input device on
this machine returned Access Denied.
Checked the system registry: microphone access is currently set to Deny.
Fix: open Settings > Privacy & security > Microphone and turn ON both
- 'Microphone access'
- 'Let desktop apps access your microphone'
then restart Micro-Vocal Lab and record again.
```

Also: on total failure `Recorder::start` now removes the header-only session WAV each
attempt left behind.

### Why this cannot be fully verified from this environment (honest gap)

This sandbox is Linux without audio hardware or a Windows VM. What *is* verified:
the ladder/classification/consent-parsing logic (17 unit tests), compilation on all 5
platforms (CI), and the real Windows registry query executing without panicking
(`consent_query_never_panics_off_windows` runs live `reg.exe` on the Windows CI leg).
What is **not** verified here: an actual `E_ACCESSDENIED` being raised and recovered on
a physical Windows 10/11 machine with the privacy toggle off → on → recording succeeds.
That requires the on-device kit (`docs/ONDEVICE.md`) on real hardware; the fix's
per-rung logging is designed to make exactly that session self-documenting.

## BUG 2 — Import panic `RefCell already borrowed` at controller.rs:977

### Root cause (exact, proven)

```rust
// v1.1.0, controller.rs ui_tick():
while let Ok(msg) = state.borrow().inbox.try_recv() {   // ← Ref temporary
    match msg {
        UiMessage::SessionReady(Ok(session)) =>
            install_session(app, state, session, None), // → state.borrow_mut() @ 977 → PANIC
```

In Rust 2021, temporaries created in a `while let` scrutinee live until the **end of the
loop body**. The `Ref` from `state.borrow()` was therefore held across the entire body,
and the first `SessionReady` message — i.e. **every real import through the dialog or
CLI** — hit `install_session`'s `borrow_mut()` on the already-shared cell and panicked
on the UI thread (process death).

Why no test caught it in v1.0.0–v1.1.0: the fuzz corpus calls `mvl_io::import` directly;
the screenshot path imports synchronously (`load_audio_sync`); the UI tests never had a
message waiting in the inbox when `refresh()` ran. The bug lived only in the async
worker→inbox→timer-drain path of the *running desktop app* — precisely where real users
landed.

### The fix (design, not a band-aid)

The channel endpoints are thread-safe crossbeam objects that never needed interior
mutability, so they moved **out of the `RefCell`** into a new `UiState` wrapper:

```rust
struct UiState {
    inner: RefCell<Inner>,                                  // window state only
    tx: crossbeam_channel::Sender<UiMessage>,               // cloned to workers
    rx: crossbeam_channel::Receiver<UiMessage>,             // drained by the timer
}
```

`ui_tick` now drains `state.rx` with **no borrow held at all** while handlers run. This
removes the entire class of re-entrancy bug structurally (the inbox can never conflict
with an `Inner` borrow again — the type system enforces it), rather than narrowing one
borrow scope.

### Proven with the user's exact crash

The new regression suite (`tests/async_import.rs`, 4 tests) drives the real user flow —
`load_path` (worker + inbox) then `refresh()` (the exact drain the 30 fps timer runs):

| | pre-fix code | fixed code |
|---|---|---|
| `async_import_installs_session_without_refcell_panic` | **panics `controller.rs:977:31: RefCell already borrowed`** (the user's report, byte-for-byte, same line & column) | passes |
| `async_import_twice_in_sequence_reuses_the_drain` | panics at 977:31 | passes |
| `rapid_refresh_burst_while_worker_runs_is_safe` | panics at 977:31 | passes |
| `async_import_error_surfaces_in_status_bar` | passes (error arm never re-borrowed) | passes |

### Controller-wide RefCell audit (proactive, as required)

All 48 borrow sites in `controller.rs` were reviewed with the borrow-lifetime rules in
mind. Findings: the `while let` scrutinee was the **only** actual double-borrow; every
other site either holds a guard over pure/computation-only code (`refresh_waveform`,
`ui_tick` part 2 — only `app.set_*` property writes, which do not re-enter Rust) or
correctly drops the guard before invoking anything (`play_pause`, `zoom`, `zoom_fit`,
`devices_clicked`). No other RefCell exists in the app crates (one test-local in
`session.rs` is fine).

## Quality gates

| Gate | Status |
|---|---|
| 1000-file fuzz → zero panics | ✅ re-run green (`fuzz_1000_random_files_zero_panics`) |
| Full suite green | ✅ **217 passed / 0 failed / 5 ignored** locally (196 + 21 new) |
| BUG 2 regression test (import path) | ✅ 4 tests, one reproducing the exact panic pre-fix |
| BUG 1 regression test (format negotiation) | ✅ 17 tests (ladder ×9, consent ×8, incl. the user's verbatim error string) |
| clippy `-D warnings` | ✅ clean |
| `cargo fmt --check` | ✅ clean |
| catch_unwind on import worker | ✅ already present since 7.1 (audited) |
| CI 5 platforms | ✅ green on tag `v1.1.1` (see below) |
| Screenshots | import-success render: `docs/phase7-evidence/ui-import-success-v1.1.1.png` (1440×900, real audio, non-neutral params). **Windows recording/import screenshots are not possible from this environment** — no Windows machine, no audio hardware, no GUI on CI runners; see honest gap above. |
| Real Windows 10/11 device testing (built-in mic / USB / Bluetooth headsets) | ❌ **not performed** — requires physical hardware; the on-device kit (`micro-vocal-lab selftest`, `docs/ONDEVICE.md`) remains the vehicle. Per-rung console logging added in this fix is designed to make that session produce directly diagnostic output. |

## Files changed

| File | Change |
|---|---|
| `crates/mvl-io/src/consent.rs` | **new** — access-denied classification, `reg.exe` consent-store query, privacy error builder (+8 tests) |
| `crates/mvl-io/src/recorder.rs` | `negotiation_ladder` (+9 tests); `start_on` walks the ladder with per-rung logging + access-denied short-circuit; `start` classifies all-denied → privacy error, cleans up the stale WAV; `SampleFmt`/`NegotiatedConfig` derive `Hash` |
| `crates/mvl-app/src/controller.rs` | `UiState` split — channels out of the `RefCell`; drain loop borrow-free; `inbox_tx` helper removed |
| `crates/mvl-app/tests/async_import.rs` | **new** — 4 regression tests for the async import path |
| `docs/phase7-evidence/ui-import-success-v1.1.1.png` | evidence render |

## Remaining gaps (honest record)

1. **No real-Windows hardware verification** (both bugs): the code paths are unit-tested
   and CI-verified on all platforms, but "recording works on Windows 10 + 11 with the
   built-in mic / USB / Bluetooth headsets, privacy off then on" can only be confirmed
   on physical machines via `docs/ONDEVICE.md`. The new per-rung logging exists so that
   if anything still fails there, the console output identifies the exact rung and
   HRESULT immediately.
2. **macOS/Linux access-denied**: the classification is generic, but only the Windows
   branch has a live registry diagnostic; other platforms fall back to the generic
   OS-permission hint (macOS has no equivalent readable toggle without TCC privileges).
3. The consent-store query reflects the *global* desktop-app toggle; individual
   per-app overrides (non-packaged apps are covered by the global toggle) and MDM
   policy layers beyond the HKLM key are not individually distinguished — the message
   covers the common cases and tells the user where to look.
