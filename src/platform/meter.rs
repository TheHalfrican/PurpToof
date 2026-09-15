//! Signal A against WASAPI.
//!
//! # Why this is session-scoped
//!
//! Measured on 2026-09-14 with the phone actually streaming: A2DP render audio
//! lands in a WASAPI session owned by a protected `svchost.exe`, **not** the
//! pid-0 audio engine. Its meter tracked the endpoint to four decimal places
//! while every other session on the endpoint read exactly `0.0`.
//!
//! That matters because the endpoint meter is maskable. In the same run,
//! another application on the PC pushed the endpoint peak to `0.0626` - over
//! 100x `silence_eps` - while A2DP was genuinely silent. An endpoint-scoped
//! Signal A would have reported healthy audio during a dead stretch.
//!
//! # How the session is identified
//!
//! Not by PID. `OpenProcess` is refused on that process even with
//! `PROCESS_QUERY_LIMITED_INFORMATION`, and "svchost" would not be unique if it
//! were not. The usable key is `GetSessionIdentifier`, which embeds the host
//! binary path - see [`crate::platform::a2dp_session::looks_like_a2dp_session`].

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
    eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::Interface;

use crate::core::traits::AudioMeter;
use crate::platform::a2dp_session::{looks_like_a2dp_session, session_identifier};

/// How often to re-hunt for the A2DP session when we do not currently hold one.
///
/// Resolution enumerates every session and reads each identifier, which
/// allocates. At the 10 Hz poll rate that would be wasteful, and the session
/// appearing is not something we need to notice within 100ms.
const RESOLVE_INTERVAL: Duration = Duration::from_secs(1);

/// Where a peak reading came from. Not part of the [`AudioMeter`] trait - the
/// state machine does not branch on it - but the UI and the log must not
/// present another application's audio as if it were the phone's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterScope {
    /// Read from the attributed A2DP session. Trustworthy.
    Session,
    /// No A2DP session has ever been attributed on this machine, so we are
    /// falling back to the endpoint. **Maskable by other applications.**
    Endpoint,
    /// A session was attributed and has since gone away. Reported as silence
    /// rather than falling back, because we know specifically that the A2DP
    /// stream is producing nothing - and falling back here is exactly how the
    /// masking failure above gets reintroduced.
    SessionGone,
}

pub struct MeterReading {
    pub peak: f32,
    pub scope: MeterScope,
}

pub struct WasapiMeter {
    endpoint: IAudioMeterInformation,
    sessions: IAudioSessionManager2,
    /// The attributed session's meter, once found.
    attributed: RefCell<Option<IAudioMeterInformation>>,
    /// Whether we have ever attributed a session on this endpoint. Drives the
    /// difference between `Endpoint` (never knew) and `SessionGone` (knew, lost
    /// it), which is the difference between an honest fallback and a lie.
    ever_attributed: Cell<bool>,
    last_resolve: RefCell<Option<Instant>>,
}

impl WasapiMeter {
    /// Bind to the current default render endpoint.
    ///
    /// The binding does not follow a later default-device change - that needs a
    /// full rebuild, which is why `DefaultDeviceChanged` is a re-arm trigger.
    ///
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn new() -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("could not create IMMDeviceEnumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no default render endpoint")?;
            let endpoint = device
                .Activate(CLSCTX_ALL, None)
                .context("could not activate IAudioMeterInformation on the endpoint")?;
            let sessions = device
                .Activate(CLSCTX_ALL, None)
                .context("could not activate IAudioSessionManager2")?;

            Ok(Self {
                endpoint,
                sessions,
                attributed: RefCell::new(None),
                ever_attributed: Cell::new(false),
                last_resolve: RefCell::new(None),
            })
        }
    }

    /// One reading, with its provenance.
    pub fn read(&self) -> MeterReading {
        if let Some(peak) = self.read_attributed() {
            return MeterReading {
                peak,
                scope: MeterScope::Session,
            };
        }

        if self.try_resolve()
            && let Some(peak) = self.read_attributed()
        {
            return MeterReading {
                peak,
                scope: MeterScope::Session,
            };
        }

        if self.ever_attributed.get() {
            // Deliberately not falling back. See MeterScope::SessionGone.
            return MeterReading {
                peak: 0.0,
                scope: MeterScope::SessionGone,
            };
        }

        MeterReading {
            peak: self.read_endpoint(),
            scope: MeterScope::Endpoint,
        }
    }

    /// Read the cached session meter, dropping it if the session has gone.
    fn read_attributed(&self) -> Option<f32> {
        let cached = self.attributed.borrow().clone();
        let meter = cached?;
        match unsafe { meter.GetPeakValue() } {
            Ok(p) => Some(p),
            Err(_) => {
                // The session died under us. Drop it so the next call
                // re-resolves rather than reading a corpse.
                *self.attributed.borrow_mut() = None;
                None
            }
        }
    }

    fn read_endpoint(&self) -> f32 {
        unsafe { self.endpoint.GetPeakValue() }.unwrap_or(0.0)
    }

    /// Hunt for the A2DP session, at most once per [`RESOLVE_INTERVAL`].
    ///
    /// Returns whether a session is now held.
    fn try_resolve(&self) -> bool {
        let now = Instant::now();
        {
            let last = self.last_resolve.borrow();
            if let Some(t) = *last
                && now.duration_since(t) < RESOLVE_INTERVAL
            {
                return false;
            }
        }
        *self.last_resolve.borrow_mut() = Some(now);

        let Some(meter) = (unsafe { self.find_a2dp_session() }) else {
            return false;
        };
        *self.attributed.borrow_mut() = Some(meter);
        self.ever_attributed.set(true);
        true
    }

    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    unsafe fn find_a2dp_session(&self) -> Option<IAudioMeterInformation> {
        unsafe {
            let enumerator = self.sessions.GetSessionEnumerator().ok()?;
            let count = enumerator.GetCount().ok()?;
            for i in 0..count {
                let Ok(control) = enumerator.GetSession(i) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                if looks_like_a2dp_session(&session_identifier(&control2))
                    && let Ok(meter) = control.cast::<IAudioMeterInformation>()
                {
                    return Some(meter);
                }
            }
            None
        }
    }
}

impl AudioMeter for WasapiMeter {
    fn peak(&self) -> f32 {
        self.read().peak
    }
}
