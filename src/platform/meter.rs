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
//!
//! # Why ALL matching sessions are held, not the first
//!
//! The 2026-09-14 measurement above found exactly one `svchost.exe` session on
//! the endpoint. That is not guaranteed. Measured again 2026-09-16, with the
//! phone freshly re-paired and genuinely streaming, there were **two**, both
//! matching the identifier rule and differing only in their grouping GUID:
//!
//! ```text
//! [2] ...\System32\svchost.exe%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}
//!       state: Inactive   peak: 0.000000
//! [7] ...\System32\svchost.exe%b{2A14476C-D8F8-455E-B260-2A2232F486EF}
//!       state: Active     peak: 1.018684     <- the actual audio
//! ```
//!
//! Binding to the first match therefore bound to the silent twin. Worse, it
//! stayed bound: `GetPeakValue` on that session *succeeds* and returns `0.0`,
//! so the "the session died, re-resolve" path never fired. The meter read
//! `0.0000` for 31 minutes while audio played at full scale, the UI showed
//! "Connected, silent", and the ladder tore the link down six times.
//!
//! So: hold every matching session and report the **maximum** peak across
//! them, and re-resolve on the interval rather than only when holding
//! nothing - the streaming session appears *later* than the idle one, so a
//! set resolved once at startup is stale by the time it matters.
//!
//! The max is also the safe direction for the residual ambiguity. If some
//! other `svchost` session ever carried unrelated audio we would read healthy
//! flow during genuine A2DP silence, which costs a missed recovery; binding to
//! the wrong one costs a false "silent" and a reconnect storm, and CLAUDE.md
//! is explicit that the storm is the worse failure.

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
    /// Every attributed session's meter. Plural by necessity - see the module
    /// docs on why the first match is not good enough.
    attributed: RefCell<Vec<IAudioMeterInformation>>,
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
                attributed: RefCell::new(Vec::new()),
                ever_attributed: Cell::new(false),
                last_resolve: RefCell::new(None),
            })
        }
    }

    /// One reading, with its provenance.
    pub fn read(&self) -> MeterReading {
        // Refresh first, on the interval. The set is resolved while the phone
        // is idle and the streaming session only appears once audio starts, so
        // a set that is never revisited is stale exactly when it matters.
        self.try_resolve();

        if let Some(peak) = self.read_attributed() {
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

    /// The loudest of the attributed session meters.
    ///
    /// `None` means we hold nothing readable, which is the caller's cue to
    /// decide between `Endpoint` and `SessionGone`. The whole set is dropped
    /// only when *every* meter in it has died, so one stale handle among live
    /// ones cannot force a spurious re-resolve.
    fn read_attributed(&self) -> Option<f32> {
        let held = self.attributed.borrow().clone();
        let mut loudest: Option<f32> = None;
        let mut all_dead = !held.is_empty();

        for meter in &held {
            if let Ok(p) = unsafe { meter.GetPeakValue() } {
                all_dead = false;
                loudest = Some(loudest.map_or(p, |best| f32::max(best, p)));
            }
        }

        if all_dead {
            // Every session died under us. Drop them so the next resolve
            // rebuilds rather than reading corpses.
            self.attributed.borrow_mut().clear();
        }
        loudest
    }

    fn read_endpoint(&self) -> f32 {
        unsafe { self.endpoint.GetPeakValue() }.unwrap_or(0.0)
    }

    /// Re-hunt for the A2DP sessions, at most once per [`RESOLVE_INTERVAL`].
    ///
    /// Runs whether or not a set is already held, because the session carrying
    /// audio appears only once the remote starts streaming - after the idle
    /// one that is already there.
    ///
    /// An enumeration that finds nothing leaves the current set alone rather
    /// than clearing it. A transient failure must not read as `SessionGone`;
    /// that determination belongs to [`Self::read_attributed`], which makes it
    /// from meters that actually refused to answer.
    fn try_resolve(&self) {
        let now = Instant::now();
        {
            let last = self.last_resolve.borrow();
            if let Some(t) = *last
                && now.duration_since(t) < RESOLVE_INTERVAL
            {
                return;
            }
        }
        *self.last_resolve.borrow_mut() = Some(now);

        let found = unsafe { self.find_a2dp_sessions() };
        if found.is_empty() {
            return;
        }
        self.ever_attributed.set(true);
        *self.attributed.borrow_mut() = found;
    }

    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    unsafe fn find_a2dp_sessions(&self) -> Vec<IAudioMeterInformation> {
        let mut found = Vec::new();
        unsafe {
            let Ok(enumerator) = self.sessions.GetSessionEnumerator() else {
                return found;
            };
            let Ok(count) = enumerator.GetCount() else {
                return found;
            };
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
                    found.push(meter);
                }
            }
        }
        found
    }
}

impl AudioMeter for WasapiMeter {
    fn peak(&self) -> f32 {
        self.read().peak
    }

    /// Re-resolve the default render endpoint and drop every cached handle.
    ///
    /// `ever_attributed` is cleared too: the new endpoint is a different
    /// machine as far as attribution is concerned, and carrying the flag over
    /// would make the meter report `SessionGone` - silence - on an endpoint
    /// where it has simply not looked yet.
    ///
    /// A failure leaves the previous binding in place. A stale meter is worse
    /// than a fresh one but far better than none, and the trigger will fire
    /// again if the device situation is still changing.
    fn rebind(&mut self) {
        match unsafe { Self::new() } {
            Ok(fresh) => *self = fresh,
            Err(e) => tracing::warn!(error = %e, "meter rebind failed, keeping the old binding"),
        }
    }
}
