//! Signal B against GSMTC.
//!
//! # This is expected to return `None` in practice
//!
//! Measured on 2026-09-14 while an iPhone 15 Pro Max was actively streaming
//! music to this PC: `GlobalSystemMediaTransportControlsSessionManager`
//! returned **no sessions at all**. Not a session with poor metadata - nothing.
//! The `Avrcp Transport` PnP node exists and publishes nothing GSMTC can see.
//!
//! So `None` is the normal result on that hardware, and the app ships degraded:
//! auto-recovery on silence stays off by default, and the UI has to say so
//! rather than pretend. Do not read a `None` here as a bug in this module.

use anyhow::{Context, Result};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinPlaybackStatus,
};

use crate::core::traits::RemotePlayback;
use crate::core::types::PlaybackStatus;

pub struct GsmtcRemote {
    manager: GlobalSystemMediaTransportControlsSessionManager,
}

impl GsmtcRemote {
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn new() -> Result<Self> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .context("GSMTC RequestAsync dispatch failed")?
            .join()
            .context("GSMTC RequestAsync did not complete")?;
        Ok(Self { manager })
    }
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
        // The current session is whichever one Windows considers foreground.
        // With no session at all this errors, which is the documented normal
        // case above and maps to None - Signal B unavailable.
        let session = self.manager.GetCurrentSession().ok()?;
        let info = session.GetPlaybackInfo().ok()?;
        translate(info.PlaybackStatus().ok()?)
    }
}
