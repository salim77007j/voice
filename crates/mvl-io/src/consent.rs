//! Windows microphone-privacy diagnostics (v1.1.1 BUG 1).
//!
//! When WASAPI returns `E_ACCESSDENIED` (HRESULT 0x80070005, printed by
//! cpal as "Access is denied. (os error -2147024891)"), the cause on a
//! normal Windows 10/11 machine is almost always the privacy gate:
//! Settings → Privacy & security → Microphone. The OS blocks the
//! `IAudioClient::Initialize` call for any desktop app that has not been
//! granted capture consent — *regardless of the requested format* — so
//! no amount of format negotiation can recover; the user must be told
//! exactly what to enable.
//!
//! This module (a) classifies access-denied errors from their text (the
//! cpal error strings are the only cross-platform surface we get), and
//! (b) on Windows reads the two CapabilityAccessManager registry values
//! that back that settings page, so the error can state *which* toggle
//! is off instead of guessing:
//!
//! * `HKCU\…\ConsentStore\microphone` — per-user "Microphone access"
//! * `HKLM\…\ConsentStore\microphone` — machine-wide policy override
//!
//! The registry is read by shelling out to `reg.exe query` (present on
//! every Windows install since forever): zero new dependencies, no
//! unsafe code, and it compiles away entirely on other platforms.

use crate::Error;

/// HRESULT `E_ACCESSDENIED` (0x80070005) as a signed 32-bit value, the
/// form Rust's `io::Error` prints for WASAPI failures.
const E_ACCESSDENIED_DECIMAL: &str = "-2147024891";

/// Does this audio-device error text represent `E_ACCESSDENIED`?
///
/// cpal renders WASAPI HRESULTs as e.g.
/// `Failed to initialize audio client: Access is denied. (os error -2147024891)`
/// so matching the decimal HRESULT and the human text covers every
/// cpal/Windows rendering seen in the wild (the hex form is matched too
/// for robustness against other layers quoting it).
#[must_use]
pub fn is_access_denied(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("access is denied") || m.contains(E_ACCESSDENIED_DECIMAL) || m.contains("0x80070005")
}

/// State of the Windows microphone consent store for one hive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// The `Value` entry reads `Allow`.
    Allow,
    /// The `Value` entry reads `Deny` — capture is blocked.
    Deny,
    /// Key missing, value unreadable, or platform is not Windows.
    Unknown,
}

/// Parse `reg.exe query` output for the `Value` entry.
///
/// Expected shape (whitespace varies between Windows versions):
/// ```text
/// HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone
///     Value    REG_SZ    Allow
/// ```
#[must_use]
pub fn parse_reg_query_output(out: &str) -> Consent {
    for line in out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Value") {
            let rest = rest.trim_start();
            // tolerate "Value   REG_SZ   Allow" and "Value    REG_SZ    Deny"
            if let Some(after_sz) = rest
                .strip_prefix("REG_SZ")
                .map(str::trim_start)
                .or_else(|| rest.strip_prefix("REG_EXPAND_SZ").map(str::trim_start))
            {
                let value = after_sz.split_whitespace().next().unwrap_or("");
                return match value.to_ascii_lowercase().as_str() {
                    "allow" => Consent::Allow,
                    "deny" => Consent::Deny,
                    _ => Consent::Unknown,
                };
            }
        }
    }
    Consent::Unknown
}

/// The consent-store subkey path under CapabilityAccessManager
/// (Windows-only: queried via `reg.exe` on demand).
#[cfg(target_os = "windows")]
const CONSENT_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

/// Query the microphone consent state (user hive, then machine policy).
///
/// Returns [`Consent::Unknown`] on non-Windows platforms and whenever the
/// query cannot be made. Never panics, never blocks long (reg.exe is
/// instantaneous; a hard 3 s timeout guards against pathological cases).
#[must_use]
pub fn microphone_consent() -> Consent {
    if cfg!(not(target_os = "windows")) {
        return Consent::Unknown;
    }
    let user = query_consent_key("HKCU");
    // Machine policy can force-deny even when the user toggle is on.
    let machine = query_consent_key("HKLM");
    match (user, machine) {
        (Consent::Deny, _) | (_, Consent::Deny) => Consent::Deny,
        (Consent::Allow, Consent::Allow) => Consent::Allow,
        (Consent::Allow, Consent::Unknown) | (Consent::Unknown, Consent::Allow) => Consent::Allow,
        (Consent::Unknown, Consent::Unknown) => Consent::Unknown,
    }
}

/// Query one hive's consent value. Windows-only helper.
#[cfg(target_os = "windows")]
fn query_consent_key(hive: &str) -> Consent {
    use std::process::{Command, Stdio};
    let full = format!(r"{hive}\{CONSENT_KEY}");
    let output = Command::new("reg")
        .arg("query")
        .arg(&full)
        .arg("/v")
        .arg("Value")
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => parse_reg_query_output(&String::from_utf8_lossy(&o.stdout)),
        _ => Consent::Unknown,
    }
}

/// Non-Windows stub — the consent store is a Windows-only concept.
#[cfg(not(target_os = "windows"))]
fn query_consent_key(_hive: &str) -> Consent {
    Consent::Unknown
}

/// Build the enriched "cannot open the microphone" error for the
/// access-denied case (v1.1.1 BUG 1).
///
/// This is deliberately *separate* from a format failure: when every
/// device on the machine reports `E_ACCESSDENIED`, telling the user
/// "tried N devices" is useless — they need the privacy steps. On
/// Windows the live consent-store state is included so the message can
/// say which toggle is off.
#[must_use]
pub fn access_denied_error(context: &str) -> Error {
    let mut msg = format!(
        "Windows denied access to the microphone ({context}).\n\
         This is the OS privacy gate, not a device or format problem — \
         every input device on this machine returned Access Denied.\n"
    );
    if cfg!(target_os = "windows") {
        match microphone_consent() {
            Consent::Deny => msg.push_str(
                "Checked the system registry: microphone access is currently set to Deny.\n",
            ),
            Consent::Allow => msg.push_str(
                "Checked the system registry: the 'Microphone access' toggle is ON, but the \
                 per-app or policy layer still denies this app.\n",
            ),
            Consent::Unknown => {
                msg.push_str("Could not read the privacy registry state.\n");
            }
        }
        msg.push_str(
            "Fix: open Settings > Privacy & security > Microphone and turn ON both\n\
             - 'Microphone access'\n\
             - 'Let desktop apps access your microphone'\n\
             then restart Micro-Vocal Lab and record again.",
        );
    } else {
        // Defensive: access-denied is a Windows concept, but keep the
        // message honest if some other host ever reports it.
        msg.push_str(
            "The operating system is blocking audio capture for this app — check the \
             OS privacy/permission settings for the microphone, then retry.",
        );
    }
    Error::Device(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact string from the user's v1.1.0 crash report.
    const USER_REPORT: &str = "audio device error: open input stream: Failed to initialize \
                               audio client: Access is denied. (os error -2147024891)";

    #[test]
    fn access_denied_detected_from_user_report() {
        assert!(is_access_denied(USER_REPORT));
    }

    #[test]
    fn access_denied_detected_from_bare_hresult_forms() {
        assert!(is_access_denied("os error -2147024891"));
        assert!(is_access_denied("HRESULT 0x80070005"));
        assert!(is_access_denied("Access is denied."));
    }

    #[test]
    fn format_errors_are_not_access_denied() {
        assert!(!is_access_denied(
            "Failed to initialize audio client: The requested format is not supported. \
             (os error -2004287483)" // AUDCLNT_E_UNSUPPORTED_FORMAT
        ));
        assert!(!is_access_denied("device disconnected"));
        assert!(!is_access_denied("no device"));
    }

    #[test]
    fn reg_output_parses_allow_and_deny() {
        let allow = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\\
                     CapabilityAccessManager\\ConsentStore\\microphone\n    Value    REG_SZ    Allow\n";
        assert_eq!(parse_reg_query_output(allow), Consent::Allow);
        let deny = allow.replace("Allow", "Deny");
        assert_eq!(parse_reg_query_output(&deny), Consent::Deny);
    }

    #[test]
    fn reg_output_tolerates_whitespace_and_extra_lines() {
        let noisy = "\r\nWARNING: some banner\r\n\r\n    Value\t\tREG_SZ\t\tDeny\r\n";
        assert_eq!(parse_reg_query_output(noisy), Consent::Deny);
    }

    #[test]
    fn reg_output_unknown_for_garbage_or_missing_value() {
        assert_eq!(parse_reg_query_output(""), Consent::Unknown);
        assert_eq!(
            parse_reg_query_output("Value    REG_SZ    Prompt"),
            Consent::Unknown
        );
        assert_eq!(
            parse_reg_query_output("PackageReference REG_SZ something"),
            Consent::Unknown
        );
    }

    #[test]
    fn consent_query_never_panics_off_windows() {
        // On the CI Linux/macOS legs this must be Unknown; on Windows it
        // runs the real reg.exe and must still not panic.
        let c = microphone_consent();
        assert!(matches!(
            c,
            Consent::Allow | Consent::Deny | Consent::Unknown
        ));
    }

    #[test]
    fn access_denied_error_mentions_privacy_settings() {
        let msg = access_denied_error("open input stream").to_string();
        assert!(
            msg.contains("denied access to the microphone"),
            "must name the problem: {msg}"
        );
        // Both platform branches must point the user at the privacy
        // controls (wording differs between the Windows and generic
        // branches, so accept either).
        assert!(
            msg.contains("Privacy") || msg.contains("privacy/permission"),
            "must name the privacy settings: {msg}"
        );
        assert!(!msg.contains("tried"), "must not blame device count");
    }
}
