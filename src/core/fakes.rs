//! Test doubles for the four OS-facing traits.
//!
//! These are test-only (`#[cfg(test)]` at the module declaration) so nothing
//! here ships in the binary.
//!
//! # The one that earns its keep
//!
//! [`FakeConnection`] can report `Opened` while [`FakeMeter`] reads zero. That
//! is the actual production bug - the Store app's failure mode, and the entire
//! reason this project exists - reproduced deterministically in a unit test.
//! Real hardware will not do it on demand, so this is the only way to grow the
//! state machine against the thing it is supposed to catch.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use crate::core::traits::{AudioMeter, Clock, RemotePlayback, SinkConnection, SinkError};
use crate::core::types::{LinkState, PlaybackStatus};

/// A clock that only moves when told to.
///
/// Every timing assertion in the suite runs on this: no `sleep()`, no flake,
/// and the whole suite stays well under a second.
pub struct FakeClock {
    base: Instant,
    offset: Cell<Duration>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Cell::new(Duration::ZERO),
        }
    }

    pub fn advance(&self, by: Duration) {
        self.offset.set(self.offset.get() + by);
    }

    pub fn advance_secs(&self, secs: u64) {
        self.advance(Duration::from_secs(secs));
    }

    pub fn advance_ms(&self, ms: u64) {
        self.advance(Duration::from_millis(ms));
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

impl Clock for &FakeClock {
    fn now(&self) -> Instant {
        (*self).now()
    }
}

/// A peak meter under test control.
pub struct FakeMeter {
    peak: Cell<f32>,
}

impl FakeMeter {
    /// Starts silent, which is the interesting default.
    pub fn silent() -> Self {
        Self {
            peak: Cell::new(0.0),
        }
    }

    pub fn with_peak(peak: f32) -> Self {
        Self {
            peak: Cell::new(peak),
        }
    }

    pub fn set(&self, peak: f32) {
        self.peak.set(peak);
    }

    /// Well above any sane `silence_eps`.
    pub fn set_audible(&self) {
        self.peak.set(0.5);
    }

    pub fn set_silent(&self) {
        self.peak.set(0.0);
    }
}

impl AudioMeter for FakeMeter {
    fn peak(&self) -> f32 {
        self.peak.get()
    }
}

/// Signal B under test control. `None` models "no session at all".
pub struct FakeRemote {
    status: Cell<Option<PlaybackStatus>>,
}

impl FakeRemote {
    pub fn playing() -> Self {
        Self {
            status: Cell::new(Some(PlaybackStatus::Playing)),
        }
    }

    pub fn with(status: Option<PlaybackStatus>) -> Self {
        Self {
            status: Cell::new(status),
        }
    }

    /// No AVRCP metadata at all - the degraded case.
    pub fn unavailable() -> Self {
        Self {
            status: Cell::new(None),
        }
    }

    pub fn set(&self, status: Option<PlaybackStatus>) {
        self.status.set(status);
    }
}

impl RemotePlayback for FakeRemote {
    fn status(&self) -> Option<PlaybackStatus> {
        self.status.get()
    }
}

/// What a scripted `open()` should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenBehavior {
    /// Open succeeds and the link transitions to `Opened`.
    Succeed,
    /// Open returns success but the link **never** transitions.
    ///
    /// This is not hypothetical: `Open()` returning `Success` while `State()`
    /// still reads `Closed` was observed on real hardware, with the
    /// transition arriving asynchronously afterwards - or, here, not at all.
    /// A recovery that ends this way is a failure, and anything that treats
    /// the `Ok(())` as success will pass its tests and break in the field.
    SucceedWithoutTransition,
    /// Open fails outright.
    Fail(SinkError),
}

/// A sink connection whose behaviour is fully scripted.
pub struct FakeConnection {
    link: Cell<LinkState>,
    /// Consumed front-to-back; the last entry repeats once exhausted.
    script: RefCell<Vec<OpenBehavior>>,
    open_calls: Cell<u32>,
    close_calls: Cell<u32>,
}

impl FakeConnection {
    /// Opens cleanly, every time.
    pub fn healthy() -> Self {
        Self::scripted(vec![OpenBehavior::Succeed])
    }

    /// Starts already open. The starting point for the silent-while-Opened
    /// scenario.
    pub fn already_open() -> Self {
        let c = Self::scripted(vec![OpenBehavior::Succeed]);
        c.link.set(LinkState::Opened);
        c
    }

    pub fn scripted(script: Vec<OpenBehavior>) -> Self {
        assert!(!script.is_empty(), "a script needs at least one behavior");
        Self {
            link: Cell::new(LinkState::Closed),
            script: RefCell::new(script),
            open_calls: Cell::new(0),
            close_calls: Cell::new(0),
        }
    }

    pub fn open_calls(&self) -> u32 {
        self.open_calls.get()
    }

    pub fn close_calls(&self) -> u32 {
        self.close_calls.get()
    }

    /// Force the link state, to model the remote dropping it underneath us.
    pub fn set_link(&self, state: LinkState) {
        self.link.set(state);
    }
}

impl SinkConnection for FakeConnection {
    fn open(&mut self) -> Result<(), SinkError> {
        self.open_calls.set(self.open_calls.get() + 1);

        let mut script = self.script.borrow_mut();
        // The final entry repeats, so a one-element script describes steady
        // behaviour without the test having to pad it.
        let behavior = if script.len() > 1 {
            script.remove(0)
        } else {
            script[0].clone()
        };

        match behavior {
            OpenBehavior::Succeed => {
                self.link.set(LinkState::Opened);
                Ok(())
            }
            OpenBehavior::SucceedWithoutTransition => {
                // Deliberately does NOT touch link state.
                Ok(())
            }
            OpenBehavior::Fail(e) => Err(e),
        }
    }

    fn close(&mut self) {
        self.close_calls.set(self.close_calls.get() + 1);
        self.link.set(LinkState::Closed);
    }

    fn link_state(&self) -> LinkState {
        self.link.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_only_moves_when_told() {
        let clock = FakeClock::new();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0, "clock must not drift on its own");
        clock.advance_secs(5);
        assert_eq!(clock.now() - t0, Duration::from_secs(5));
        clock.advance_ms(500);
        assert_eq!(clock.now() - t0, Duration::from_millis(5_500));
    }

    /// THE test. The production bug, on demand.
    ///
    /// A connection that says `Opened` while the meter says silence is
    /// precisely what the Store app does before it needs a manual restart.
    /// Everything else in the health model exists to catch this.
    #[test]
    fn connection_can_report_opened_while_the_meter_reads_zero() {
        let connection = FakeConnection::already_open();
        let meter = FakeMeter::silent();
        let remote = FakeRemote::playing();

        assert_eq!(
            connection.link_state(),
            LinkState::Opened,
            "the link claims to be open"
        );
        assert_eq!(meter.peak(), 0.0, "yet no sound is coming out");
        assert_eq!(
            remote.status(),
            Some(PlaybackStatus::Playing),
            "and the phone believes it is playing"
        );
    }

    #[test]
    fn succeed_without_transition_leaves_the_link_closed() {
        let mut c = FakeConnection::scripted(vec![OpenBehavior::SucceedWithoutTransition]);
        assert_eq!(c.link_state(), LinkState::Closed);
        assert!(c.open().is_ok(), "open reports success");
        assert_eq!(
            c.link_state(),
            LinkState::Closed,
            "but the link never actually opened - the observed hardware behaviour"
        );
    }

    #[test]
    fn a_failing_open_surfaces_its_error() {
        let mut c = FakeConnection::scripted(vec![OpenBehavior::Fail(SinkError::Denied)]);
        assert_eq!(c.open(), Err(SinkError::Denied));
        assert_eq!(c.link_state(), LinkState::Closed);
    }

    #[test]
    fn scripts_advance_then_repeat_the_last_entry() {
        let mut c = FakeConnection::scripted(vec![
            OpenBehavior::Fail(SinkError::TimedOut),
            OpenBehavior::Succeed,
        ]);
        assert!(c.open().is_err(), "first entry");
        assert!(c.open().is_ok(), "second entry");
        c.close();
        assert!(c.open().is_ok(), "last entry repeats once exhausted");
        assert_eq!(c.open_calls(), 3);
        assert_eq!(c.close_calls(), 1);
    }

    #[test]
    fn close_returns_the_link_to_closed() {
        let mut c = FakeConnection::healthy();
        c.open().unwrap();
        assert_eq!(c.link_state(), LinkState::Opened);
        c.close();
        assert_eq!(c.link_state(), LinkState::Closed);
        assert_eq!(c.close_calls(), 1);
    }

    #[test]
    fn link_can_be_dropped_underneath_us() {
        let c = FakeConnection::already_open();
        assert_eq!(c.link_state(), LinkState::Opened);
        c.set_link(LinkState::Closed);
        assert_eq!(
            c.link_state(),
            LinkState::Closed,
            "models the remote going away without us asking"
        );
    }

    #[test]
    fn meter_and_remote_are_independently_controllable() {
        let meter = FakeMeter::silent();
        let remote = FakeRemote::unavailable();
        assert_eq!(meter.peak(), 0.0);
        assert_eq!(remote.status(), None, "no session at all");

        meter.set_audible();
        remote.set(Some(PlaybackStatus::Paused));
        assert!(meter.peak() > 0.1);
        assert_eq!(remote.status(), Some(PlaybackStatus::Paused));
    }
}
