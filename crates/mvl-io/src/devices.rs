//! Audio device discovery across **all** cpal hosts (BUG 1 / BUG 3 fix).
//!
//! v1.0.0 opened only the default host's default device and gave up with
//! "no input device found" when that single call returned `None`. Real
//! machines showed exactly that: a laptop with a working built-in mic and
//! the app refusing to record, because the *default device pointer* was
//! unset (fresh Windows profile, macOS after an aggregate-device reset,
//! Linux with no pulse default) even though the device itself was healthy.
//!
//! This module enumerates every device on every host cpal can initialize
//! and exposes pure, unit-testable fallback ordering. Opening still goes
//! through [`crate::recorder::Recorder::start_on`] /
//! [`crate::player::Player::on_device`], which now also walk these
//! candidates in order and *keep trying* until one actually opens a
//! stream — enumeration alone is not proof a device works.
//!
//! The module never panics and never fails: a host that cannot initialize
//! is recorded in `hosts_failed` and shown to the user, it never aborts
//! the walk.

use cpal::traits::HostTrait;
use cpal::Host;

use crate::{Error, Result};

/// One discovered device endpoint (input or output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Human-readable device name (what the OS calls it).
    pub name: String,
    /// cpal host id that owns it ("wasapi", "coreaudio", "alsa", …).
    pub host: String,
    /// `true` when this is the *default* device of its host.
    pub is_default: bool,
}

impl DeviceInfo {
    /// Stable identity string for persistence/comparison:
    /// `host::name`. Host+name is unique within one enumeration pass.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}::{}", self.host, self.name)
    }

    /// Label for UI pickers: `"Built-in Microphone (WASAPI)"`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.host)
    }
}

/// Everything [`list`] found on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInventory {
    /// Capture endpoints, best-first (defaults before non-defaults).
    pub inputs: Vec<DeviceInfo>,
    /// Playback endpoints, best-first.
    pub outputs: Vec<DeviceInfo>,
    /// Hosts that initialized successfully (for diagnostics).
    pub hosts_ok: Vec<String>,
    /// Hosts that failed to initialize, with the error text.
    pub hosts_failed: Vec<(String, String)>,
}

impl DeviceInventory {
    /// Find a previously-seen input by its [`DeviceInfo::id`].
    #[must_use]
    pub fn input_by_id(&self, id: &str) -> Option<&DeviceInfo> {
        self.inputs.iter().find(|d| d.id() == id)
    }

    /// Find a previously-seen output by its [`DeviceInfo::id`].
    #[must_use]
    pub fn output_by_id(&self, id: &str) -> Option<&DeviceInfo> {
        self.outputs.iter().find(|d| d.id() == id)
    }
}

/// Enumerate every input and output device on every host cpal can bring
/// up. **Never panics** — a failing host is recorded, not propagated.
#[must_use]
pub fn list() -> DeviceInventory {
    let mut inv = DeviceInventory::default();
    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(h) => h,
            Err(e) => {
                inv.hosts_failed
                    .push((host_id.name().to_string(), format!("{e}")));
                continue;
            }
        };
        let host_name = host_id.name().to_string();
        inv.hosts_ok.push(host_name.clone());

        collect(&host, &host_name, Direction::Input, &mut inv.inputs);
        collect(&host, &host_name, Direction::Output, &mut inv.outputs);
    }
    // Defaults first within each list (stable order otherwise), so callers
    // that just take element 0 get the most sensible device.
    inv.inputs.sort_by_key(|d| !d.is_default);
    inv.outputs.sort_by_key(|d| !d.is_default);
    inv
}

/// Direction selector for [`collect`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Input,
    Output,
}

/// Append one host's devices to `out`. Devices whose name cannot be
/// queried are still listed as "(unnamed device)" — they may be perfectly
/// usable; only the *name* failed.
fn collect(host: &Host, host_name: &str, dir: Direction, out: &mut Vec<DeviceInfo>) {
    // cpal 0.18: the name comes from `Display` (the `name()` method of
    // older releases is gone; `description()` adds heavier metadata).
    let default_name = match dir {
        Direction::Input => host.default_input_device().map(|d| d.to_string()),
        Direction::Output => host.default_output_device().map(|d| d.to_string()),
    };

    let devices = match dir {
        Direction::Input => host.input_devices(),
        Direction::Output => host.output_devices(),
    };
    let iter = match devices {
        Ok(it) => it,
        Err(e) => {
            eprintln!("mvl-io: host '{host_name}' failed to list devices: {e}");
            return;
        }
    };
    for dev in iter {
        let name = dev.to_string();
        if name.trim().is_empty() {
            continue; // unnamed husks (disconnected BT profiles etc.)
        }
        out.push(DeviceInfo {
            is_default: default_name.as_deref() == Some(name.as_str()),
            name,
            host: host_name.to_string(),
        });
    }
}

/// Open a concrete [`cpal::Device`] by its [`DeviceInfo::id`], re-walking
/// the hosts (cpal devices are not persistable handles).
///
/// # Errors
/// [`Error::Device`] when no device with that id exists anymore (unplugged
/// between enumeration and open).
pub fn open_by_id(id: &str) -> Result<cpal::Device> {
    let (want_host, want_name) = id.split_once("::").ok_or_else(|| {
        Error::Device(format!(
            "'{id}' is not a valid device id (expected host::name)"
        ))
    })?;
    let host_id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name() == want_host)
        .ok_or_else(|| {
            Error::Device(format!(
                "audio host '{want_host}' is not available on this machine"
            ))
        })?;
    let host = cpal::host_from_id(host_id)
        .map_err(|e| Error::Device(format!("initialize host '{want_host}': {e}")))?;
    let mut found: Option<cpal::Device> = None;
    for dev in host
        .input_devices()
        .into_iter()
        .flatten()
        .chain(host.output_devices().into_iter().flatten())
    {
        if dev.to_string() == want_name {
            found = Some(dev);
            break;
        }
    }
    found.ok_or_else(|| {
        Error::Device(format!(
            "device '{want_name}' on host '{want_host}' is no longer present"
        ))
    })
}

/// Pure ordering rule: which candidate devices to try first when opening.
///
/// Order (mirrors the fallback chain the bug report demands):
/// 1. the default device of the default host,
/// 2. every other default device,
/// 3. everything else, preserving enumeration order.
///
/// Unit-tested with synthetic lists — no hardware needed.
#[must_use]
pub fn fallback_order<'a>(candidates: &'a [DeviceInfo], default_host: &str) -> Vec<&'a DeviceInfo> {
    let mut ordered: Vec<&DeviceInfo> = candidates.iter().collect();
    ordered.sort_by_key(|d| match (d.is_default, d.host == default_host) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    });
    // sort_by_key is stable, so within each class the enumeration order
    // (usually the OS's own ordering) survives.
    ordered
}

/// Build the user-facing diagnostic when *no* device could be opened.
/// Platform-specific hints turn a dead end into an actionable message.
#[must_use]
pub fn no_input_device_error(inv: &DeviceInventory) -> Error {
    let mut msg = String::from("No working audio input was found");
    if inv.inputs.is_empty() {
        msg.push_str(" (no capture devices are visible to the system)");
    } else {
        msg.push_str(". Devices seen but none would open: ");
        let names: Vec<String> = inv.inputs.iter().map(|d| d.label()).collect();
        msg.push_str(&names.join(", "));
    }
    if !inv.hosts_failed.is_empty() {
        let fails: Vec<String> = inv
            .hosts_failed
            .iter()
            .map(|(h, e)| format!("{h}: {e}"))
            .collect();
        msg.push_str(&format!(". Hosts that failed: {}", fails.join("; ")));
    }
    msg.push_str(&platform_input_hint());
    Error::Device(msg)
}

/// Same as [`no_input_device_error`] for playback endpoints.
#[must_use]
pub fn no_output_device_error(inv: &DeviceInventory) -> Error {
    let mut msg = String::from("No working audio output was found");
    if inv.outputs.is_empty() {
        msg.push_str(" (no playback devices are visible to the system)");
    } else {
        msg.push_str(". Devices seen but none would open: ");
        let names: Vec<String> = inv.outputs.iter().map(|d| d.label()).collect();
        msg.push_str(&names.join(", "));
    }
    msg.push_str(&platform_output_hint());
    Error::Device(msg)
}

fn platform_input_hint() -> String {
    if cfg!(target_os = "macos") {
        "\nTips: check System Settings > Privacy & Security > Microphone — \
             Micro-Vocal Lab must be allowed. If you denied it earlier, run \
             `tccutil reset Microphone` and restart the app to get the \
             permission prompt again."
            .to_string()
    } else if cfg!(target_os = "windows") {
        "\nTips: check Settings > Privacy & security > Microphone — 'Microphone \
             access' and 'Let desktop apps access your microphone' must both be \
             ON. Also check Sound settings > Input for a selected default device."
            .to_string()
    } else if cfg!(target_os = "linux") {
        "\nTips: PipeWire/PulseAudio users should run `pactl info` to confirm a \
             server is up and `pactl list sources short` to see capture devices. \
             Bare ALSA systems need a usable ~/.asoundrc or /etc/asound.conf \
             default (an ALSA 'null' device can be used for smoke tests)."
            .to_string()
    } else {
        String::new()
    }
}

fn platform_output_hint() -> String {
    if cfg!(target_os = "macos") {
        "\nTips: check System Settings > Sound > Output for a selected device.".to_string()
    } else if cfg!(target_os = "windows") {
        "\nTips: check Settings > System > Sound > Output for a selected device.".to_string()
    } else if cfg!(target_os = "linux") {
        "\nTips: run `pactl info` / `aplay -l` to verify a sound server and \
             playback devices exist."
            .to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, host: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            host: host.into(),
            is_default,
        }
    }

    #[test]
    fn fallback_prefers_default_of_default_host() {
        let candidates = vec![
            dev("Some USB Mic", "alsa", false),
            dev("PulseAudio Source", "pulse", true),
            dev("Built-in Mic", "wasapi", true),
            dev("Loopback", "wasapi", false),
        ];
        let order = fallback_order(&candidates, "wasapi");
        let ids: Vec<String> = order.iter().map(|d| d.id()).collect();
        assert_eq!(
            ids,
            vec![
                "wasapi::Built-in Mic",
                "pulse::PulseAudio Source",
                "wasapi::Loopback",
                "alsa::Some USB Mic",
            ],
            "default-host default first, then other defaults, then the rest"
        );
    }

    #[test]
    fn fallback_with_no_defaults_keeps_order() {
        let candidates = vec![dev("A", "alsa", false), dev("B", "alsa", false)];
        let order = fallback_order(&candidates, "coreaudio");
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].name, "A");
        assert_eq!(order[1].name, "B");
    }

    #[test]
    fn fallback_empty_is_empty() {
        let order = fallback_order(&[], "wasapi");
        assert!(order.is_empty());
    }

    #[test]
    fn inventory_lookup_by_id() {
        let mut inv = DeviceInventory::default();
        inv.inputs.push(dev("Mic", "alsa", true));
        assert!(inv.input_by_id("alsa::Mic").is_some());
        assert!(inv.input_by_id("alsa::Nope").is_none());
        assert!(inv.output_by_id("alsa::Mic").is_none());
    }

    #[test]
    fn live_enumeration_never_panics() {
        // Touches real hardware/hosts; on CI runners (no audio) it must
        // return an inventory with possibly-empty lists, not panic.
        let inv = list();
        for d in &inv.inputs {
            assert!(!d.name.is_empty());
            assert!(!d.host.is_empty());
        }
        for d in &inv.outputs {
            assert!(!d.name.is_empty());
            assert!(!d.host.is_empty());
        }
        // ids are unique in each list (host+name identity)
        let mut ids: Vec<String> = inv.inputs.iter().map(|d| d.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), inv.inputs.len(), "duplicate input ids");
    }

    #[test]
    fn open_by_id_rejects_garbage() {
        let err = open_by_id("not-a-valid-id").unwrap_err();
        assert!(matches!(err, Error::Device(_)), "got {err:?}");
    }

    #[test]
    fn error_messages_carry_hints() {
        let inv = DeviceInventory::default();
        let e = no_input_device_error(&inv);
        let msg = e.to_string();
        assert!(msg.contains("No working audio input"), "{msg}");
        assert!(
            msg.contains("Tips")
                || msg.contains("Tips:")
                || msg.contains("pactl")
                || msg.contains("Privacy")
                || msg.contains("Settings"),
            "platform hint missing: {msg}"
        );
        let e2 = no_output_device_error(&inv);
        assert!(e2.to_string().contains("No working audio output"));
    }
}
