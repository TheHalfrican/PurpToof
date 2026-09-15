//! Identifying the WASAPI session that A2DP audio renders into.
//!
//! Shared by the real [`crate::platform::meter::WasapiMeter`] and by
//! `--debug-sessions`, so the rule lives in exactly one place.

use windows::Win32::Media::Audio::IAudioSessionControl2;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::PWSTR;

/// Whether a session identifier looks like the A2DP render session.
///
/// Observed on the rig while the phone streamed:
///
/// ```text
/// {0.0.0.00000000}.{a1b9084c-...}|\Device\HarddiskVolume2\Windows\System32\svchost.exe%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}
/// ```
///
/// The trailing GUID is the session grouping param, and it appears in the
/// *session* identifier rather than only the *instance* identifier - which
/// suggests a fixed GUID for the Bluetooth audio render service rather than a
/// per-connection random. It was byte-identical across every sample taken.
///
/// **It is deliberately not matched on.** Its stability across a reconnect, a
/// reboot, or another machine has never been established, and a matcher that
/// silently stops matching is worse than one that is slightly loose. Matching
/// the host binary path is enough to exclude every ordinary application, and a
/// caller that finds no match must fall back to the endpoint meter rather than
/// conclude the stream is dead.
pub fn looks_like_a2dp_session(identifier: &str) -> bool {
    identifier
        .to_ascii_lowercase()
        .contains(r"\system32\svchost.exe")
}

/// `GetSessionIdentifier`, rendered and freed.
///
/// Returns an empty string on failure, which
/// [`looks_like_a2dp_session`] correctly rejects.
pub fn session_identifier(control: &IAudioSessionControl2) -> String {
    unsafe { pwstr_field(|| control.GetSessionIdentifier()) }
}

/// `GetSessionInstanceIdentifier`, rendered and freed. Diagnostics only.
pub fn session_instance_identifier(control: &IAudioSessionControl2) -> String {
    unsafe { pwstr_field(|| control.GetSessionInstanceIdentifier()) }
}

/// Render a `PWSTR`-returning getter, freeing the string afterwards.
///
/// These allocate with `CoTaskMemAlloc` and the caller owns the result, so one
/// named helper is the only place that ownership rule has to be right. Keeping
/// HRESULT- and PWSTR-handling calls each behind a single named helper is how
/// this layer stays too dumb to be wrong in an interesting way - the
/// `IsSystemSoundsSession` bug recorded in `docs/verify.md` is what happens
/// otherwise.
///
/// # Safety
///
/// `f` must return a `PWSTR` that the caller owns, or an error.
unsafe fn pwstr_field(f: impl FnOnce() -> windows::core::Result<PWSTR>) -> String {
    match f() {
        Ok(p) if !p.is_null() => unsafe {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        },
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_a2dp_session;

    /// The identifier observed on the rig while the phone was streaming.
    const A2DP: &str = concat!(
        r"{0.0.0.00000000}.{a1b9084c-5158-4f1e-83ee-848cb39fdf12}|",
        r"\Device\HarddiskVolume2\Windows\System32\svchost.exe",
        r"%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}"
    );

    #[test]
    fn matches_the_observed_a2dp_session() {
        assert!(looks_like_a2dp_session(A2DP));
    }

    #[test]
    fn does_not_match_ordinary_applications() {
        // Every other session present on the endpoint during that run.
        for other in [
            r"{0.0.0.00000000}.{a1b9084c}|\Device\HarddiskVolume2\Program Files (x86)\Microsoft\EdgeWebView\Application\152.0.4191.66\msedgewebview2.exe%b{0}",
            r"{0.0.0.00000000}.{a1b9084c}|\Device\HarddiskVolume2\Program Files (x86)\Steam\steam.exe%b{0}",
            r"{0.0.0.00000000}.{a1b9084c}|\Device\HarddiskVolume5\SteamLibrary\steamapps\common\Call of Duty 4\iw3sp.exe%b{0}",
            r"{0.0.0.00000000}.{a1b9084c}|#%b{145C0C6C-1D2B-4943-8737-CE3B23EF0401}",
            r"{0.0.0.00000000}.{a1b9084c}|#%b{A9EF3FD9-4240-455E-A4D5-F2B3301887B2}",
        ] {
            assert!(!looks_like_a2dp_session(other), "wrongly matched: {other}");
        }
    }

    #[test]
    fn is_case_insensitive_on_the_path() {
        assert!(looks_like_a2dp_session(
            r"x|\Device\HarddiskVolume2\WINDOWS\SYSTEM32\SVCHOST.EXE%b{C55CBD10}"
        ));
    }

    #[test]
    fn an_unreadable_identifier_is_rejected_rather_than_matched() {
        // session_identifier returns "" when the call fails. Matching on that
        // would attribute A2DP audio to an arbitrary session.
        assert!(!looks_like_a2dp_session(""));
    }

    #[test]
    fn does_not_match_svchost_outside_system32() {
        // Guards against the match being so loose that any path containing the
        // word svchost qualifies.
        assert!(!looks_like_a2dp_session(
            r"x|\Device\HarddiskVolume2\Temp\svchost.exe%b{0}"
        ));
    }
}
