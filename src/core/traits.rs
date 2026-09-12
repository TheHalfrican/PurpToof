//! The seam between pure logic and Windows.
//!
//! `core/` depends on these traits and never on windows-rs. `platform/`
//! implements them for real; tests implement them as fakes. The rule that
//! keeps this honest: **if a function contains a branch, it belongs in
//! `core/`.** The interop layer should be too dumb to be wrong in an
//! interesting way.

use std::time::Instant;

use crate::core::types::{LinkState, PlaybackStatus};

/// Signal A - is sound actually coming out.
///
/// Backed by `IAudioMeterInformation::GetPeakValue` on the default render
/// endpoint, or on the A2DP render session if it turns out to be attributable
/// to a process.
pub trait AudioMeter {
    /// Peak amplitude since the last read, 0.0 ..= 1.0.
    fn peak(&self) -> f32;
}

/// Signal B - does the remote think it is playing.
///
/// Backed by `GlobalSystemMediaTransportControlsSessionManager`, which reaches
/// us over AVRCP.
pub trait RemotePlayback {
    /// `None` means no session exists at all - Signal B is unavailable, which
    /// is a different thing from a session reporting `Closed`.
    fn status(&self) -> Option<PlaybackStatus>;
}

/// The A2DP sink itself.
pub trait SinkConnection {
    /// Advertise and open the stream.
    ///
    /// Returning `Ok` means the open call succeeded, **not** that the link is
    /// open - callers must wait for [`SinkConnection::link_state`] to reach
    /// `Opened`, because the transition arrives asynchronously.
    fn open(&mut self) -> Result<(), SinkError>;

    /// Release the connection. Explicit rather than relying on `Drop`.
    fn close(&mut self);

    fn link_state(&self) -> LinkState;
}

/// Why opening the sink failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// The system refused us - `DeniedBySystem`.
    Denied,
    /// The remote never showed up in time - `RequestTimedOut`.
    TimedOut,
    /// No such device, or it went away mid-open.
    DeviceUnavailable,
    /// Anything else, with the underlying detail preserved for the log.
    Other(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Denied => write!(f, "denied by system"),
            SinkError::TimedOut => write!(f, "request timed out"),
            SinkError::DeviceUnavailable => write!(f, "device unavailable"),
            SinkError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for SinkError {}

/// Time, injected so tests never sleep.
///
/// The whole suite runs on a fake clock: no `sleep()`, no flake, and the
/// timing-dependent behaviour (silence timeout, backoff ladder, debounce,
/// ladder reset) becomes directly assertable instead of approximately
/// observable.
pub trait Clock {
    fn now(&self) -> Instant;
}

/// The real one.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
