//! The vocabulary the health state machine reasons in.
//!
//! Nothing here touches Windows. These are deliberate mirrors of the WinRT
//! enums rather than re-exports, so `core/` stays compilable and testable
//! without windows-rs, and so the translation happens in exactly one place
//! (`platform/`) where it can be wrong in only one way.

use std::time::Duration;

/// What the remote device says it is doing.
///
/// A faithful mirror of WinRT's
/// `GlobalSystemMediaTransportControlsSessionPlaybackStatus`, which has **six**
/// variants - not the four an earlier sketch of the decision table assumed.
/// `Changing` in particular is a transient and must never be read as a fault.
///
/// The seventh case - "there is no session at all", i.e. Signal B is
/// unavailable - is represented as `Option::None` rather than a variant here,
/// because it is a statement about the *absence* of a session and not a status
/// a session can hold. See [`Observation::remote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Closed,
    Opened,
    Changing,
    Stopped,
    Playing,
    Paused,
}

impl PlaybackStatus {
    /// Every variant, for exhaustive table-driven tests.
    pub const ALL: [PlaybackStatus; 6] = [
        PlaybackStatus::Closed,
        PlaybackStatus::Opened,
        PlaybackStatus::Changing,
        PlaybackStatus::Stopped,
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
    ];

    /// Whether the remote is asserting that audio should be coming out right
    /// now. Only `Playing` qualifies: this is the load-bearing half of the
    /// decision rule, and widening it is how false-positive reconnects start.
    pub fn asserts_audio(self) -> bool {
        matches!(self, PlaybackStatus::Playing)
    }
}

/// Link state of the A2DP sink.
///
/// Mirrors `AudioPlaybackConnectionState`. This is a **link** signal, not a
/// flow signal - the entire premise of this project is that it can read
/// `Opened` while no audio is moving. Never decide health from it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Closed,
    Opened,
}

/// Why the lifecycle is being re-run without it counting as a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReArmReason {
    /// The link closed on its own. Observed in practice during ordinary use,
    /// so this is expected rather than a failure - but the app cannot idle in
    /// `Closed` or audio never resumes.
    LinkClosed,
    /// An external event says the world changed underneath us.
    Trigger(Trigger),
}

/// Why a genuine recovery is being attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryReason {
    /// The actual bug this project exists to fix: the remote says it is
    /// playing and the endpoint has been silent past the timeout.
    SilentWhilePlaying { silent_for: Duration },
    /// Re-arming kept failing to produce healthy flow. Escalated so the
    /// backoff ladder applies and we stop reopening in a tight loop.
    ReArmExhausted { attempts: u32 },
}

/// Events that should re-run the lifecycle independently of the watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Resume from sleep or modern standby. Fires in overlapping bursts,
    /// which is why triggers are debounced.
    Resume,
    /// Bluetooth radio toggled off and on.
    RadioToggled,
    /// Default render endpoint changed. Needs a full reopen, not a UI
    /// refresh: the render target is bound when the connection opens and does
    /// not follow the system default.
    DefaultDeviceChanged,
    /// The device appeared or disappeared on the playback-connection selector.
    DeviceChanged,
    /// The user pressed Reconnect. Never debounced away entirely - a person
    /// pressing a button deserves a response.
    Manual,
}

/// What the state machine decides to do about an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Do nothing. The overwhelmingly common case, and the correct answer for
    /// every false-positive scenario.
    None,
    /// Reopen the connection. Benign and expected: no backoff, no entry in
    /// the reconnect log, no toast.
    ReArm(ReArmReason),
    /// Audio is genuinely dead. Reopen, write it to the reconnect log, and
    /// let the backoff ladder space the next attempt.
    Recover(RecoveryReason),
}

impl Action {
    pub fn is_none(self) -> bool {
        matches!(self, Action::None)
    }

    /// Whether this action belongs in the user-visible reconnect log. The
    /// distinction is the whole point of splitting re-arm from recovery: a log
    /// full of benign reopens every time a song ends tells the user nothing.
    pub fn is_loggable(self) -> bool {
        matches!(self, Action::Recover(_))
    }
}

/// How a recovery attempt turned out.
///
/// This exists because `Open()` returning `Success` does **not** mean the link
/// is open - the transition to `Opened` arrives asynchronously afterwards. A
/// recovery is only successful once that transition is actually observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The link reached `Opened`.
    Succeeded,
    /// The open failed, or returned success but never transitioned within the
    /// timeout. Both are failures and both advance the ladder.
    Failed,
}

/// One sample of the world, as seen from outside the connection object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    /// Link state of our own sink connection.
    pub link: LinkState,
    /// Signal B. `None` means no media session exists at all, i.e. the remote
    /// publishes nothing over AVRCP and Signal B is unavailable. That is a
    /// distinct case from any `Some(..)` status, including `Closed`.
    pub remote: Option<PlaybackStatus>,
    /// Signal A. Peak amplitude from the render meter, 0.0 ..= 1.0.
    pub peak: f32,
}

/// The honest status line, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// No link.
    Disconnected,
    /// Link open and audio is moving. The good state.
    Streaming,
    /// Link open, endpoint quiet, and that is fine - paused, stopped, or a
    /// genuine quiet passage.
    ConnectedSilent,
    /// Recovering, with the attempt number so the user can see it working.
    Reconnecting { attempt: u32 },
    /// Running without Signal B. Auto-recovery is restricted; say so rather
    /// than pretending everything is normal.
    Degraded,
}
