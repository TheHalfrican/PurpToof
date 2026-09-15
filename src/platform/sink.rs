//! The A2DP sink itself, against `Windows.Media.Audio.AudioPlaybackConnection`.
//!
//! # Always armed
//!
//! `Start()` is held for the lifetime of this object and `Open()` is reissued
//! the instant the link closes. That is not defensive coding, it *is* the
//! feature. Measured on 2026-09-14: an always-armed sink held a link `Opened`
//! for 300 seconds across an app switch, the source app being killed outright,
//! 97 seconds of continuous silence, and a different app starting playback -
//! one arm, zero drops, with iOS routing the new app's audio to the PC
//! unprompted.
//!
//! The app this project replaces is intermittent, and the most likely reason is
//! that it treats `Closed` as a resting state and waits for the user.
//!
//! # Two corrections to the CLAUDE.md lifecycle sketch
//!
//! 1. `Open()` returning `Success` does **not** mean the link is open. The
//!    transition to `Opened` arrives asynchronously afterwards, and `State()`
//!    still reads `Closed` in between. [`Sink::open`] therefore reports success
//!    on dispatch only; the caller waits for [`Sink::link_state`].
//! 2. **"No remote" is not a failure, and does not arrive as
//!    `RequestTimedOut`.** With the phone switched off, `Open()` returns
//!    `UnknownFailure` with extended error `0x8007001F` after 0.8-4.9s,
//!    identically every time; `RequestTimedOut` has never been observed at
//!    all. Both map to non-fatal errors that the caller turns into
//!    `RecoveryOutcome::NoRemote` rather than a failed re-arm. See
//!    [`classify_unknown_failure`].

use anyhow::{Context, Result, bail};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::TypedEventHandler;
use windows::Media::Audio::{
    AudioPlaybackConnection, AudioPlaybackConnectionOpenResultStatus, AudioPlaybackConnectionState,
};

use crate::core::traits::{SinkConnection, SinkError};
use crate::core::types::LinkState;

/// A paired device that can act as an A2DP source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkDevice {
    pub id: String,
    pub name: String,
}

/// Enumerate paired A2DP source devices.
///
/// Matching only already-paired devices is a property of the selector, not of
/// this function - an unpaired phone will not appear no matter what.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
pub unsafe fn list_devices() -> Result<Vec<SinkDevice>> {
    let selector = AudioPlaybackConnection::GetDeviceSelector()
        .context("GetDeviceSelector failed - class did not activate")?;
    let found = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .context("FindAllAsyncAqsFilter dispatch failed")?
        .join()
        .context("FindAllAsyncAqsFilter did not complete")?;

    let mut out = Vec::new();
    for device in &found {
        out.push(SinkDevice {
            id: device.Id().unwrap_or_default().to_string(),
            name: device.Name().unwrap_or_default().to_string(),
        });
    }
    Ok(out)
}

/// `HRESULT_FROM_WIN32(ERROR_GEN_FAILURE)` - "a device attached to the system
/// is not functioning."
///
/// What `Open()` reports, via the extended error, when the remote's radio is
/// off or it is out of range. Measured 2026-09-14 with the phone's Bluetooth
/// switched off: identical on every one of 25 consecutive attempts.
const E_DEVICE_NOT_FUNCTIONING: i32 = 0x8007_001Fu32 as i32;

/// Turn an `UnknownFailure` into something the state machine can act on.
///
/// Split out and tested because the distinction is load-bearing: one of these
/// must not escalate and the other must. Inlining it at the call site is
/// exactly the shape of mistake this layer is supposed to be too dumb to make.
fn classify_unknown_failure(extended: Option<windows::core::HRESULT>) -> SinkError {
    match extended {
        Some(h) if h.0 == E_DEVICE_NOT_FUNCTIONING => SinkError::Unreachable,
        Some(h) => SinkError::Other(format!("Open failed, extended error {:#010x}", h.0)),
        None => SinkError::Other("Open failed with no extended error".into()),
    }
}

pub struct Sink {
    connection: AudioPlaybackConnection,
    state_token: i64,
    /// Whether `Start()` has been called. Held for the object's lifetime once
    /// set, so the PC never stops advertising as a sink.
    started: bool,
    device: SinkDevice,
}

impl Sink {
    /// Construct against a specific device id.
    ///
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn connect(device: SinkDevice) -> Result<Self> {
        let connection = AudioPlaybackConnection::TryCreateFromId(&device.id.as_str().into())
            .with_context(|| format!("TryCreateFromId failed for {}", device.name))?;

        // Link-state transitions are logged, never used alone to decide health
        // - the premise of this project is that this signal can read `Opened`
        // while nothing comes out.
        let state_token = connection
            .StateChanged(&TypedEventHandler::<
                AudioPlaybackConnection,
                windows::core::IInspectable,
            >::new(|sender, _| {
                if let Some(s) = sender.as_ref() {
                    tracing::debug!(state = ?s.State(), "AudioPlaybackConnection StateChanged");
                }
                Ok(())
            }))
            .context("StateChanged registration failed")?;

        Ok(Self {
            connection,
            state_token,
            started: false,
            device,
        })
    }

    /// Construct against the first paired device, if any.
    ///
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn connect_first() -> Result<Self> {
        let devices = unsafe { list_devices() }?;
        let Some(device) = devices.into_iter().next() else {
            bail!(
                "no A2DP source devices found. Pair a phone with this PC first; \
                 the selector only matches already-paired devices."
            );
        };
        unsafe { Sink::connect(device) }
    }

    pub fn device(&self) -> &SinkDevice {
        &self.device
    }

    /// Advertise this PC as an available sink. Idempotent.
    ///
    /// Held for the object's lifetime - see the module docs.
    fn ensure_started(&mut self) -> Result<(), SinkError> {
        if self.started {
            return Ok(());
        }
        self.connection
            .Start()
            .map_err(|e| SinkError::Other(format!("Start failed: {e}")))?;
        self.started = true;
        Ok(())
    }
}

impl SinkConnection for Sink {
    fn open(&mut self) -> Result<(), SinkError> {
        self.ensure_started()?;

        let result = self
            .connection
            .Open()
            .map_err(|e| SinkError::Other(format!("Open dispatch failed: {e}")))?;
        let status = result
            .Status()
            .map_err(|e| SinkError::Other(format!("Open result had no status: {e}")))?;

        // One named mapping per status, rather than an `is_ok()` at the call
        // site. Both of the interesting values here are non-fatal in different
        // ways, and conflating them is how a phone in another room starts
        // looking like a hardware fault.
        if status == AudioPlaybackConnectionOpenResultStatus::Success {
            // Dispatch succeeded. The link is NOT open yet - the caller waits
            // for link_state() to reach Opened.
            Ok(())
        } else if status == AudioPlaybackConnectionOpenResultStatus::RequestTimedOut {
            Err(SinkError::TimedOut)
        } else if status == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem {
            Err(SinkError::Denied)
        } else {
            // UnknownFailure is not one thing. The extended error is the only
            // way to tell "the phone's radio is off" from a genuine fault, and
            // getting this wrong makes an absent phone escalate.
            Err(classify_unknown_failure(result.ExtendedError().ok()))
        }
    }

    fn close(&mut self) {
        // Explicit rather than relying on Drop, per CLAUDE.md.
        let _ = self.connection.Close();
    }

    fn link_state(&self) -> LinkState {
        match self.connection.State() {
            Ok(s) if s == AudioPlaybackConnectionState::Opened => LinkState::Opened,
            // Closed, an unknown variant, or an error all mean "not usable".
            // Reporting Opened on an error would be the one genuinely
            // dangerous mistake this function could make.
            _ => LinkState::Closed,
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.connection.Close();
        let _ = self.connection.RemoveStateChanged(self.state_token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    #[test]
    fn a_powered_off_radio_is_unreachable_not_a_fault() {
        // The measured signature. If this mapping regresses, an absent phone
        // silently starts ratcheting the backoff ladder again.
        assert_eq!(
            classify_unknown_failure(Some(HRESULT(E_DEVICE_NOT_FUNCTIONING))),
            SinkError::Unreachable
        );
    }

    #[test]
    fn any_other_extended_error_stays_a_real_failure() {
        // E_ACCESSDENIED, picked because it is emphatically not "come back
        // later" - treating it as Unreachable would retry forever in silence.
        let other = classify_unknown_failure(Some(HRESULT(0x8007_0005u32 as i32)));
        assert!(matches!(other, SinkError::Other(_)), "got {other:?}");
    }

    #[test]
    fn a_missing_extended_error_stays_a_real_failure() {
        let none = classify_unknown_failure(None);
        assert!(matches!(none, SinkError::Other(_)), "got {none:?}");
    }

    #[test]
    fn the_message_keeps_the_hresult_readable() {
        // The log is the only place a novel failure gets diagnosed, so the
        // code has to survive into it.
        let SinkError::Other(msg) = classify_unknown_failure(Some(HRESULT(0x8007_0005u32 as i32)))
        else {
            panic!("expected Other");
        };
        assert!(msg.contains("0x80070005"), "unhelpful message: {msg}");
    }
}
