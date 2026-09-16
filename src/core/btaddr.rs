//! Pulling a Bluetooth device address out of a Windows device-interface id.
//!
//! # Why this exists
//!
//! The id we hold for a device is the **A2DP sink interface** id, the one
//! `AudioPlaybackConnection::GetDeviceSelector()` matches. Observed on this
//! machine 2026-09-16:
//!
//! ```text
//! \\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}_VID&0001004c_PID&761e#7&2df5970d&0&644842670BF4_C00000000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK
//! ```
//!
//! `BluetoothDevice::FromIdAsync` will not take that - it wants a
//! `BluetoothDevice` id, which is a different namespace. The one thing that
//! ties the two together is the 48-bit device address embedded in the
//! interface id (`644842670BF4` above), and
//! `BluetoothDevice::FromBluetoothAddressAsync` takes exactly that as a `u64`.
//!
//! # Why it is in `core/`
//!
//! It branches, so CLAUDE.md's rule puts it here rather than in `platform/`.
//! That is not a technicality: the parse below has a genuine ambiguity trap
//! (see [`address_from_interface_id`]) which is worth having under test on
//! real observed ids, and none of it needs Windows to run.
//!
//! # What this is for
//!
//! Reading the **classic BR/EDR** link state separately from the LE one. A
//! phone pairs twice, over two independent bonds, and only the classic bond
//! carries A2DP.
//!
//! That two-bond structure is not a guess. `BTHUSB` event 10 on 2026-09-16
//! removed two link keys for the same phone in the same second - the classic
//! address and the LE one - and event 8 re-minted them separately a minute
//! later, the LE half under a fresh resolvable private address:
//!
//! ```text
//! 18:33:12  Id 10  link key removed  64:48:42:67:0b:f4   classic
//! 18:33:12  Id 10  link key removed  5b:95:56:a6:f7:ec   LE
//! 18:34:51  Id  8  paired            64:48:42:67:0b:f4   classic
//! 18:34:52  Id  8  paired            76:cf:be:13:df:ad   LE, new RPA
//! ```
//!
//! During the outage that prompted this module, the classic half was
//! unusable while the LE half stayed connected, so Windows kept displaying
//! "Connected" while no audio path could exist. **Why** the classic half
//! stopped authenticating is still open - there were only two `BTHUSB` event
//! 16 failures, 43s apart, and then none across eight hours of the app
//! retrying, which means later attempts failed before authentication was
//! ever reached. Telling the two bonds apart is what this module is for; it
//! does not claim to explain what broke one of them.

/// A 48-bit Bluetooth device address.
///
/// Stored as the `u64` that `FromBluetoothAddressAsync` expects, rather than
/// six bytes, so the interop layer has nothing left to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BtAddr(u64);

impl BtAddr {
    /// The value `FromBluetoothAddressAsync` takes.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Build from a raw value, rejecting anything wider than 48 bits.
    ///
    /// A too-wide value means the parse picked up something that was not an
    /// address, and silently truncating it would produce a plausible-looking
    /// address for the wrong device.
    pub fn from_u64(v: u64) -> Option<Self> {
        (v <= 0xFFFF_FFFF_FFFF).then_some(Self(v))
    }

    /// Colon-separated lower case, the form the Windows event log uses.
    ///
    /// `BTHUSB` event 16 reports authentication failures as
    /// `(64:48:42:67:0b:f4)`. Matching that spelling exactly means a log line
    /// from this app can be grepped against the system log without
    /// reformatting either one.
    pub fn to_colon_string(self) -> String {
        let b = self.0.to_be_bytes();
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[2], b[3], b[4], b[5], b[6], b[7]
        )
    }
}

/// Extract the device address from a Windows device-interface or instance id.
///
/// # The ambiguity trap
///
/// The obvious implementation - "find twelve hex digits in a row" - is wrong,
/// and wrong in a way that yields a confident answer rather than an error. The
/// interface id above contains three such runs, and two of them are the final
/// node of a GUID:
///
/// ```text
/// {0000110a-0000-1000-8000-00805f9b34fb}   ->  00805f9b34fb
/// {6994ad04-93ef-11d0-a3cc-00a0c9223196}   ->  00a0c9223196
/// 7&2df5970d&0&644842670BF4_C00000000      ->  644842670BF4   <- the real one
/// ```
///
/// A naive scan returns the A2DP service GUID's node and every lookup then
/// fails against a device that does not exist. So GUID spans are removed
/// before tokenising, and an id that still yields two *different* candidates
/// is treated as unparseable rather than guessed at - see below.
///
/// # Why ambiguity returns `None`
///
/// The risk is asymmetric, the same way it is in `platform/remote.rs`.
/// Returning `None` costs us one diagnostic signal and the caller degrades to
/// not reporting classic link state. Returning the *wrong* address makes us
/// read some other device's connection status and report it as this phone's,
/// which would put a confident falsehood in front of the user - precisely the
/// misleading-"Connected" problem this is meant to cure.
pub fn address_from_interface_id(id: &str) -> Option<BtAddr> {
    let without_guids = strip_guid_spans(id);

    let mut found: Option<u64> = None;
    for token in without_guids.split(|c: char| !c.is_ascii_alphanumeric()) {
        let Some(value) = parse_address_token(token) else {
            continue;
        };
        match found {
            // The same address written twice is common and fine: the classic
            // instance id spells it under both `DEV_` and `BLUETOOTHDEVICE_`.
            Some(seen) if seen == value => {}
            // Two genuinely different candidates. We cannot tell which is the
            // device, so we do not pretend to.
            Some(_) => return None,
            None => found = Some(value),
        }
    }

    found.and_then(BtAddr::from_u64)
}

/// A token is an address only if it is exactly twelve hex digits.
///
/// Length is the whole test. The neighbouring tokens in a real id are 4, 8 or
/// 9 hex digits (`761e`, `2df5970d`, `C00000000`), so twelve is unambiguous
/// once GUIDs are out of the way.
fn parse_address_token(token: &str) -> Option<u64> {
    if token.len() != 12 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(token, 16).ok()
}

/// Remove every `{...}` span.
///
/// Unterminated braces drop the rest of the string. That is deliberate: an id
/// with a dangling brace is malformed, and the conservative reading is to
/// extract nothing from the remainder rather than to resume scanning inside
/// what may still be a GUID.
fn strip_guid_spans(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut depth = 0usize;
    for c in id.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real A2DP sink interface id for the phone this project targets,
    /// copied verbatim from `--debug-sessions` output on 2026-09-16.
    const SNK_INTERFACE_ID: &str = "\\\\?\\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}_VID&0001004c_PID&761e#7&2df5970d&0&644842670BF4_C00000000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\\SNK";

    const IPHONE: u64 = 0x6448_4267_0BF4;

    #[test]
    fn the_real_sink_interface_id_parses() {
        let addr = address_from_interface_id(SNK_INTERFACE_ID).expect("should parse");
        assert_eq!(addr.as_u64(), IPHONE);
    }

    #[test]
    fn guid_nodes_are_not_mistaken_for_the_address() {
        // This is the entire reason the function is not a one-line regex.
        // Both of these are twelve hex digits sitting inside a GUID, and a
        // naive scan returns the first one.
        let addr = address_from_interface_id(SNK_INTERFACE_ID).expect("should parse");
        assert_ne!(
            addr.as_u64(),
            0x0080_5f9b_34fb,
            "picked up the A2DP service GUID's node"
        );
        assert_ne!(
            addr.as_u64(),
            0x00a0_c922_3196,
            "picked up the interface class GUID's node"
        );
    }

    #[test]
    fn the_classic_instance_id_parses_too() {
        // Observed via Get-PnpDevice on 2026-09-16. Spells the address twice,
        // which must not read as an ambiguity.
        let id = "BTHENUM\\DEV_644842670BF4\\7&2DF5970D&0&BLUETOOTHDEVICE_644842670BF4";
        assert_eq!(
            address_from_interface_id(id).map(BtAddr::as_u64),
            Some(IPHONE)
        );
    }

    #[test]
    fn case_does_not_matter() {
        let lower = "BTHENUM\\DEV_644842670bf4";
        let upper = "BTHENUM\\DEV_644842670BF4";
        assert_eq!(
            address_from_interface_id(lower),
            address_from_interface_id(upper)
        );
    }

    #[test]
    fn two_different_addresses_are_refused_rather_than_guessed() {
        // Better to lose the signal than to report another device's link
        // state as this phone's.
        let id = "BTHENUM\\DEV_644842670BF4\\7&0&5B9556A6F7EC";
        assert_eq!(address_from_interface_id(id), None);
    }

    #[test]
    fn an_id_with_no_address_yields_nothing() {
        for id in [
            "",
            "BTHENUM",
            // Only GUIDs, which is all that is left of a sink id once the
            // instance segment is missing.
            "#{0000110a-0000-1000-8000-00805f9b34fb}#{6994ad04-93ef-11d0-a3cc-00a0c9223196}",
            // Eight and nine hex digits: the neighbours that must not match.
            "7&2df5970d&0&C00000000",
        ] {
            assert_eq!(
                address_from_interface_id(id),
                None,
                "{id:?} should not parse"
            );
        }
    }

    #[test]
    fn an_unterminated_guid_swallows_the_rest() {
        // Malformed input must not resume scanning inside what may still be a
        // GUID and surface its node as an address.
        let id = "BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb#644842670BF4";
        assert_eq!(address_from_interface_id(id), None);
    }

    #[test]
    fn addresses_wider_than_48_bits_are_refused() {
        assert_eq!(BtAddr::from_u64(0x1_0000_0000_0000), None);
        assert!(BtAddr::from_u64(0xFFFF_FFFF_FFFF).is_some());
    }

    #[test]
    fn colon_form_matches_the_event_log_spelling() {
        // BTHUSB event 16 on 2026-09-15 reported exactly this string, and the
        // point of matching it is that a log line from this app and a line
        // from the system log can be compared without reformatting either.
        let addr = BtAddr::from_u64(IPHONE).unwrap();
        assert_eq!(addr.to_colon_string(), "64:48:42:67:0b:f4");
    }

    #[test]
    fn round_trip_through_the_parser_and_back_to_text() {
        let addr = address_from_interface_id(SNK_INTERFACE_ID).unwrap();
        assert_eq!(addr.to_colon_string(), "64:48:42:67:0b:f4");
    }
}
