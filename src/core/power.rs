//! Whether the Bluetooth radio is allowed to power itself down.
//!
//! # What this reads, and why it is not WMI
//!
//! Device Manager's *"Allow the computer to turn off this device to save
//! power"* checkbox is surfaced by the `MSPower_DeviceEnable` WMI class, which
//! is what most documentation points at. It is readable without elevation, but
//! reaching it from Rust means `IWbemLocator`, `ConnectServer`, `ExecQuery` and
//! a `VARIANT` unwrap - a lot of interop for one boolean, in the layer that is
//! supposed to be too dumb to be wrong.
//!
//! Located empirically 2026-09-16 instead: unticking that box writes
//!
//! ```text
//! HKLM\SYSTEM\CurrentControlSet\Enum\<instance id>\Device Parameters\WDF
//!     IdleInWorkingState = 0
//! ```
//!
//! which is the WDF idle policy behind the checkbox. A `REG_DWORD` read is
//! something `platform/` can do in a dozen lines and get right.
//!
//! # Why an absent value does not warn
//!
//! `IdleInWorkingState` absent means "no explicit policy, the driver's default
//! applies", and this project has not established what that default is on any
//! adapter - only that unticking writes `0`. The ticked-by-default case was
//! never observed as a registry value, because by the time anyone looked it had
//! already been turned off.
//!
//! So absent is reported as [`RadioPowerPolicy::Unknown`] and does **not**
//! raise the banner. The alternative - warning whenever we cannot prove the
//! radio is held on - would put a confident claim about the user's hardware in
//! front of them on the strength of a missing registry value. That is the same
//! trade `platform/remote.rs` makes for an unidentified GSMTC session, and the
//! same one `core/btaddr.rs` makes for an ambiguous address: when the evidence
//! does not support the claim, make no claim.
//!
//! The cost is a missed warning on an adapter that has never been touched.
//! CLAUDE.md files this whole feature as "optional, low priority", so a
//! conservative miss is the right side to fail on.

/// What the radio's idle policy permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioPowerPolicy {
    /// Windows is permitted to power the adapter down. This is the state
    /// CLAUDE.md's "Radio power management" section warns about.
    MayPowerDown,
    /// The adapter is explicitly held on.
    HeldOn,
    /// No policy value, or it could not be read. Says nothing either way.
    Unknown,
}

impl RadioPowerPolicy {
    /// Whether the UI should raise the power-management banner.
    ///
    /// Only a positively-read "may power down" qualifies. See the module docs
    /// on why `Unknown` stays silent.
    pub fn should_warn(self) -> bool {
        self == Self::MayPowerDown
    }
}

/// Interpret the `IdleInWorkingState` value.
///
/// `None` is an absent or unreadable value, not a zero - the distinction is
/// the whole point, so the caller must not collapse it on the way in.
pub fn classify(idle_in_working_state: Option<u32>) -> RadioPowerPolicy {
    match idle_in_working_state {
        Some(0) => RadioPowerPolicy::HeldOn,
        // Any non-zero is the policy being enabled. Treating only `1` as
        // enabled would let an unexpected value read as "held on", which is
        // the one direction that fails silently.
        Some(_) => RadioPowerPolicy::MayPowerDown,
        None => RadioPowerPolicy::Unknown,
    }
}

/// Turn a WinRT device-interface id into the devnode instance id that the
/// `Enum` registry path is keyed by.
///
/// Interface ids come in two spellings for the same device. WinRT hands out
/// the Win32 namespace form, while the devnode's own `SymbolicLinkName` uses
/// the NT object form - both observed for this adapter on 2026-09-16:
///
/// ```text
/// \\?\USB#VID_8087&PID_0033#5&3b72e4cf&0&14#{0850302a-b344-4fda-9be9-90576b8d46f0}
/// \??\USB#VID_8087&PID_0033#5&3b72e4cf&0&14#{0850302a-b344-4fda-9be9-90576b8d46f0}
/// ```
///
/// while the registry wants
///
/// ```text
/// USB\VID_8087&PID_0033\5&3b72e4cf&0&14
/// ```
///
/// The transformation is: drop either prefix, drop the trailing GUID, and swap
/// `#` for `\`. It lives here rather than in `platform/` because it branches,
/// and because getting it wrong would silently build a registry path that
/// simply does not exist - which reads identically to "the value is absent",
/// i.e. it would disable the warning rather than fail loudly.
pub fn devnode_id_from_interface_id(interface_id: &str) -> Option<String> {
    let trimmed = interface_id
        .strip_prefix(r"\\?\")
        .or_else(|| interface_id.strip_prefix(r"\??\"))
        .unwrap_or(interface_id)
        .trim_end_matches('\\');

    let mut parts: Vec<&str> = trimmed.split('#').collect();

    // The trailing interface class GUID is not part of the devnode id.
    if parts.last().is_some_and(|p| p.starts_with('{')) {
        parts.pop();
    }

    // Enumerator, device id, instance id. Fewer than three means this is not
    // an interface id we understand, and guessing at a registry path is worse
    // than declining to look.
    if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }

    Some(parts.join("\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Intel adapter on the target machine, read verbatim from its
    /// devnode's `SymbolicLinkName` on 2026-09-16. `{0850302a-...}` is the
    /// Bluetooth radio interface class.
    const ADAPTER_IFACE_NT: &str =
        r"\??\USB#VID_8087&PID_0033#5&3b72e4cf&0&14#{0850302a-b344-4fda-9be9-90576b8d46f0}";
    /// The same interface in the Win32 namespace, which is the spelling WinRT
    /// hands back from a device enumeration.
    const ADAPTER_IFACE_WIN32: &str =
        r"\\?\USB#VID_8087&PID_0033#5&3b72e4cf&0&14#{0850302a-b344-4fda-9be9-90576b8d46f0}";
    const ADAPTER_DEVNODE: &str = r"USB\VID_8087&PID_0033\5&3b72e4cf&0&14";

    #[test]
    fn both_spellings_of_the_real_adapter_id_convert_identically() {
        // The NT form is what the devnode stores; the Win32 form is what
        // WinRT returns. They must not disagree, or the warning would work
        // from one code path and silently not from the other.
        assert_eq!(
            devnode_id_from_interface_id(ADAPTER_IFACE_NT).as_deref(),
            Some(ADAPTER_DEVNODE)
        );
        assert_eq!(
            devnode_id_from_interface_id(ADAPTER_IFACE_WIN32).as_deref(),
            Some(ADAPTER_DEVNODE)
        );
    }

    #[test]
    fn an_id_without_the_prefix_still_converts() {
        let id = r"USB#VID_8087&PID_0033#5&3b72e4cf&0&14#{0850302a-b344-4fda-9be9-90576b8d46f0}";
        assert_eq!(
            devnode_id_from_interface_id(id).as_deref(),
            Some(ADAPTER_DEVNODE)
        );
    }

    #[test]
    fn an_id_without_a_trailing_guid_is_left_alone() {
        let id = r"\\?\USB#VID_8087&PID_0033#5&3b72e4cf&0&14";
        assert_eq!(
            devnode_id_from_interface_id(id).as_deref(),
            Some(ADAPTER_DEVNODE)
        );
    }

    #[test]
    fn a_malformed_id_declines_rather_than_building_a_wrong_path() {
        // A bad path reads back as "value absent", which would silently
        // disable the warning instead of failing where anyone can see it.
        for id in [
            "",
            "USB",
            "USB#VID_8087",
            r"\\?\{92383b0e-f90e-4ac9-8d44-8c2d0d0ebda2}",
            // An empty segment would produce `USB\\5&...`, a path that cannot
            // match anything.
            "USB##5&3b72e4cf&0&14",
        ] {
            assert_eq!(
                devnode_id_from_interface_id(id),
                None,
                "{id:?} should not convert"
            );
        }
    }

    #[test]
    fn zero_means_the_radio_is_held_on() {
        // What unticking the box writes, observed 2026-09-16.
        assert_eq!(classify(Some(0)), RadioPowerPolicy::HeldOn);
        assert!(!classify(Some(0)).should_warn());
    }

    #[test]
    fn any_nonzero_means_it_may_power_down() {
        for v in [1, 2, 0xFFFF_FFFF] {
            assert_eq!(classify(Some(v)), RadioPowerPolicy::MayPowerDown);
            assert!(classify(Some(v)).should_warn(), "{v} should warn");
        }
    }

    #[test]
    fn an_absent_value_is_unknown_and_stays_quiet() {
        // The load-bearing case. We have never observed the ticked-by-default
        // state as a registry value, so absence is not evidence of it.
        assert_eq!(classify(None), RadioPowerPolicy::Unknown);
        assert!(!classify(None).should_warn());
    }
}
