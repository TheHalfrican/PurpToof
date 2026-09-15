//! Signal B against GSMTC.
//!
//! # This must only ever report the REMOTE DEVICE's session
//!
//! GSMTC surfaces media sessions from applications running on this PC as well
//! as from Bluetooth devices over AVRCP. `GetCurrentSession()` returns whichever
//! Windows considers foreground, which on a desktop is almost always a local
//! app - a browser tab, a music player.
//!
//! Using it produced a real reconnect storm on 2026-09-15:
//!
//! ```text
//! 03:16:16  Recover(SilentWhilePlaying { silent_for: 3.0503468s })
//! 03:16:20  Recover(SilentWhilePlaying { silent_for: 3.0435362s })
//! 03:16:24  Recover(SilentWhilePlaying { silent_for: 3.0473045s })
//! ```
//!
//! A browser on the PC reported `Playing`, the A2DP meter correctly reported
//! silence because the phone was paused, and the watchdog concluded the audio
//! path was dead. That is precisely the false-positive CLAUDE.md names as worse
//! than the bug this project exists to fix.
//!
//! # Why an unidentified session reports `None`
//!
//! The risk is asymmetric. A false `Playing` authorises a reconnect and costs
//! the user their audio; a false `None` only drops us into degraded mode, where
//! auto-recovery on silence is off and the manual button still works. So a
//! session is used **only** when it can be positively tied to the connected
//! device, and anything else is `None`.
//!
//! # Expect `None` on the target hardware
//!
//! Measured 2026-09-14 while an iPhone 15 Pro Max streamed to this PC: GSMTC
//! returned no session from the phone at all. `None` is the normal result here.
//! Do not read it as a bug in this module.

use anyhow::{Context, Result};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinPlaybackStatus,
};

use crate::core::traits::RemotePlayback;
use crate::core::types::PlaybackStatus;

pub struct GsmtcRemote {
    manager: GlobalSystemMediaTransportControlsSessionManager,
    /// Normalised name of the device we are a sink for. A session is only
    /// trusted when its source app id can be tied back to this.
    device_key: String,
}

impl GsmtcRemote {
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn new(device_name: &str) -> Result<Self> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .context("GSMTC RequestAsync dispatch failed")?
            .join()
            .context("GSMTC RequestAsync did not complete")?;
        Ok(Self {
            manager,
            device_key: normalise(device_name),
        })
    }

    /// Every session's source app id, for `--debug-sessions` and for learning
    /// what a genuine remote-device session looks like if one ever appears.
    pub fn source_app_ids(&self) -> Vec<String> {
        let Ok(sessions) = self.manager.GetSessions() else {
            return Vec::new();
        };
        sessions
            .into_iter()
            .filter_map(|s| s.SourceAppUserModelId().ok())
            .map(|id| id.to_string())
            .collect()
    }
}

/// Strip everything that varies between how a name is written and how an app
/// id spells it: case, spaces, apostrophes, punctuation.
fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a GSMTC source app id plausibly belongs to the connected device.
///
/// Deliberately conservative: it requires the device's own name to appear in
/// the id. It will miss a device whose session is published under an opaque id
/// (a MAC address, a GUID), and that is the intended trade - see the module
/// docs on asymmetric risk.
fn is_device_session(source_app_id: &str, device_key: &str) -> bool {
    if device_key.is_empty() {
        return false;
    }
    normalise(source_app_id).contains(device_key)
}

/// Translate the WinRT status newtype.
///
/// windows-rs generates these as associated consts on a tuple struct, so they
/// cannot be used as match patterns and `Debug` prints a bare integer. One
/// named mapping, in one place - `Changing` in particular is a transient and
/// must never be folded into something it is not.
fn translate(s: WinPlaybackStatus) -> Option<PlaybackStatus> {
    if s == WinPlaybackStatus::Closed {
        Some(PlaybackStatus::Closed)
    } else if s == WinPlaybackStatus::Opened {
        Some(PlaybackStatus::Opened)
    } else if s == WinPlaybackStatus::Changing {
        Some(PlaybackStatus::Changing)
    } else if s == WinPlaybackStatus::Stopped {
        Some(PlaybackStatus::Stopped)
    } else if s == WinPlaybackStatus::Playing {
        Some(PlaybackStatus::Playing)
    } else if s == WinPlaybackStatus::Paused {
        Some(PlaybackStatus::Paused)
    } else {
        // An unknown variant is not a status we can reason about. Reporting it
        // as anything concrete would put a guess into the decision table.
        None
    }
}

impl RemotePlayback for GsmtcRemote {
    fn status(&self) -> Option<PlaybackStatus> {
        let sessions = self.manager.GetSessions().ok()?;
        for session in &sessions {
            let Ok(id) = session.SourceAppUserModelId() else {
                continue;
            };
            if !is_device_session(&id.to_string(), &self.device_key) {
                // A local application. Its playback state says nothing about
                // whether the phone is sending us audio.
                continue;
            }
            let info = session.GetPlaybackInfo().ok()?;
            return translate(info.PlaybackStatus().ok()?);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{is_device_session, normalise};

    const IPHONE: &str = "noahsiphone";

    #[test]
    fn local_applications_are_never_treated_as_the_remote() {
        // Every one of these was observed in a real GSMTC enumeration on this
        // machine. Trusting any of them caused the reconnect storm recorded in
        // the module docs.
        for id in [
            "Helium.TL7FSSFXV44M357KD5SIY7AQBE",
            "Microsoft.YouTube_8wekyb3d8bbwe!App",
            "Spotify.exe",
            "chrome.exe",
            "msedge.exe",
        ] {
            assert!(
                !is_device_session(id, IPHONE),
                "{id} must not be mistaken for the phone"
            );
        }
    }

    #[test]
    fn a_session_naming_the_device_is_accepted() {
        for id in [
            "Noah's iPhone",
            "NoahsiPhone",
            "BluetoothAvrcp_Noahs_iPhone",
            "noahs-iphone",
        ] {
            assert!(is_device_session(id, IPHONE), "{id} should match");
        }
    }

    #[test]
    fn an_unknown_device_name_matches_nothing() {
        // Guards the degenerate case: an empty key would make `contains`
        // true for every session and reintroduce the bug wholesale.
        assert!(!is_device_session("Spotify.exe", ""));
        assert!(!is_device_session("", ""));
    }

    #[test]
    fn normalisation_ignores_punctuation_and_case() {
        assert_eq!(normalise("Noah's iPhone"), "noahsiphone");
        assert_eq!(normalise("NOAH-S_IPHONE"), "noahsiphone");
    }
}
