//! Wiring: one tick of "look at the world, ask the state machine, do as told".
//!
//! This is generic over the four traits rather than over WinRT, which is what
//! makes it testable. All of it runs on the fake clock in the tests below; the
//! real binary supplies `platform/`'s implementations and a 10 Hz sleep.
//!
//! # Why an open is not resolved on the same tick it is dispatched
//!
//! `Open()` returning success does **not** mean the link is open - the
//! transition arrives asynchronously, and `State()` reads `Closed` in between.
//! So a dispatched open enters an *awaiting* phase, and only reaching
//! `Opened` counts as success. Running out of
//! [`Config::open_transition_timeout`] without the transition is a failure,
//! which is precisely the case `OpenBehavior::SucceedWithoutTransition` exists
//! to reproduce.
//!
//! # Why "no remote" is not a failure
//!
//! With the sink permanently armed, `Open()` keeps failing for as long as the
//! phone is out of range. That is the steady state, not a fault, so it resolves
//! as [`RecoveryOutcome::NoRemote`] and never touches the ladder.
//!
//! What that failure actually looks like was measured on 2026-09-14 with the
//! phone's Bluetooth switched off, and it is **not** `RequestTimedOut` as the
//! design assumed. It is `UnknownFailure` carrying `0x8007001F`, which
//! `platform/` maps to [`SinkError::Unreachable`]. Before that mapping existed
//! it fell into `SinkError::Other` and escalated, ratcheting the backoff ladder
//! against a phone that was simply switched off. Both spellings are handled
//! here; they stay distinct only so the log can say which occurred.

use std::time::Instant;

use crate::core::config::Config;
use crate::core::health::HealthMonitor;
use crate::core::traits::{AudioMeter, Clock, RemotePlayback, SinkConnection, SinkError};
use crate::core::types::{Action, HealthStatus, LinkState, Observation, RecoveryOutcome, Trigger};

/// What one [`Supervisor::tick`] did. Returned for logging and tests; the
/// caller is not required to branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Nothing needed doing. The overwhelmingly common case.
    Idle,
    /// An open is in flight and the link has not transitioned yet.
    AwaitingOpen,
    /// The state machine asked for something and it was dispatched.
    Dispatched(Action),
    /// An in-flight open reached a verdict.
    Resolved(RecoveryOutcome),
}

pub struct Supervisor<S, M, R, C>
where
    S: SinkConnection,
    M: AudioMeter,
    R: RemotePlayback,
    C: Clock + Clone,
{
    monitor: HealthMonitor<C>,
    clock: C,
    sink: S,
    meter: M,
    remote: R,
    /// When the in-flight open was dispatched, if there is one. `Some` is the
    /// single-flight guard for the open itself; `HealthMonitor` separately
    /// suppresses new decisions while it believes an operation is running.
    awaiting_open: Option<Instant>,
}

impl<S, M, R, C> Supervisor<S, M, R, C>
where
    S: SinkConnection,
    M: AudioMeter,
    R: RemotePlayback,
    C: Clock + Clone,
{
    pub fn new(clock: C, config: Config, sink: S, meter: M, remote: R) -> Self {
        Self {
            monitor: HealthMonitor::new(clock.clone(), config),
            clock,
            sink,
            meter,
            remote,
            awaiting_open: None,
        }
    }

    pub fn status(&self) -> HealthStatus {
        self.monitor.status()
    }

    pub fn note_trigger(&mut self, trigger: Trigger) {
        self.monitor.note_trigger(trigger);
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The meter, for callers that want more than the trait exposes - the real
    /// one can also report whether a reading came from the attributed A2DP
    /// session or from the maskable endpoint fallback. The state machine
    /// deliberately does not branch on that; the UI must.
    pub fn meter(&self) -> &M {
        &self.meter
    }

    /// Sample the world once and act on it.
    pub fn tick(&mut self) -> Tick {
        let obs = self.observe();

        if let Some(started) = self.awaiting_open {
            // Bookkeeping still runs while awaiting - the monitor suppresses
            // decisions on its own, and skipping observations here would leave
            // its silence window stale.
            self.monitor.observe(obs);

            if obs.link == LinkState::Opened {
                return self.resolve(RecoveryOutcome::Succeeded);
            }

            let waited = self.clock.now().saturating_duration_since(started);
            if waited >= self.monitor.config().open_transition_timeout() {
                // Success on dispatch, no transition. A failed arm.
                return self.resolve(RecoveryOutcome::Failed);
            }

            return Tick::AwaitingOpen;
        }

        match self.monitor.observe(obs) {
            Action::None => Tick::Idle,
            action => {
                self.dispatch();
                Tick::Dispatched(action)
            }
        }
    }

    fn observe(&self) -> Observation {
        Observation {
            link: self.sink.link_state(),
            remote: self.remote.status(),
            peak: self.meter.peak(),
        }
    }

    /// Tear the connection down and open it again.
    ///
    /// `close()` is called unconditionally and explicitly rather than relying
    /// on `Drop`, per CLAUDE.md - reopening without closing is how a stale
    /// half-open connection survives a recovery that was supposed to replace
    /// it.
    fn dispatch(&mut self) {
        self.sink.close();
        match self.sink.open() {
            Ok(()) => self.awaiting_open = Some(self.clock.now()),
            // Both mean "no remote", and neither is a fault. They are kept
            // distinct only so the log can say which actually happened - on
            // real hardware a powered-off phone produces Unreachable, and
            // RequestTimedOut has never yet been observed at all.
            Err(SinkError::TimedOut | SinkError::Unreachable) => {
                self.monitor.recovery_finished(RecoveryOutcome::NoRemote);
            }
            Err(_) => {
                self.monitor.recovery_finished(RecoveryOutcome::Failed);
            }
        }
    }

    fn resolve(&mut self, outcome: RecoveryOutcome) -> Tick {
        self.awaiting_open = None;
        self.monitor.recovery_finished(outcome);
        Tick::Resolved(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fakes::{FakeClock, FakeConnection, FakeMeter, FakeRemote, OpenBehavior};
    use crate::core::types::{PlaybackStatus, RecoveryReason};

    fn config() -> Config {
        Config::default()
    }

    fn sup(
        clock: &FakeClock,
        sink: FakeConnection,
        meter: FakeMeter,
        remote: FakeRemote,
    ) -> Supervisor<FakeConnection, FakeMeter, FakeRemote, &FakeClock> {
        Supervisor::new(clock, config(), sink, meter, remote)
    }

    /// The single most valuable test in the repo, now end to end: the link
    /// reports `Opened`, the meter reads zero, the remote says it is playing.
    /// That is the production bug, and the supervisor must actually reopen.
    #[test]
    fn opened_while_silent_and_playing_eventually_reopens() {
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::silent(),
            FakeRemote::playing(),
        );

        // Below the timeout: nothing must happen.
        clock.advance_secs(1);
        assert_eq!(s.tick(), Tick::Idle);
        clock.advance_secs(1);
        assert_eq!(s.tick(), Tick::Idle);

        // Past it: a real recovery, not a benign re-arm.
        clock.advance_secs(5);
        match s.tick() {
            Tick::Dispatched(Action::Recover(RecoveryReason::SilentWhilePlaying { .. })) => {}
            other => panic!("expected a recovery, got {other:?}"),
        }
        assert_eq!(s.sink().open_calls(), 1);
        assert_eq!(s.sink().close_calls(), 1, "must close before reopening");
    }

    #[test]
    fn a_healthy_stream_is_left_alone() {
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::with_peak(0.4),
            FakeRemote::playing(),
        );

        for _ in 0..100 {
            clock.advance_ms(100);
            assert_eq!(s.tick(), Tick::Idle);
        }
        assert_eq!(s.sink().open_calls(), 0, "never touch a working stream");
        assert_eq!(s.status(), HealthStatus::Streaming);
    }

    #[test]
    fn success_without_a_transition_resolves_as_failed() {
        // Open() said Success, State() never reached Opened. Treating the
        // Ok(()) as success is the mistake this models.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::SucceedWithoutTransition]),
            FakeMeter::silent(),
            FakeRemote::playing(),
        );

        clock.advance_secs(1);
        assert!(matches!(s.tick(), Tick::Dispatched(_)));

        // Awaiting, not yet resolved.
        clock.advance_ms(100);
        assert_eq!(s.tick(), Tick::AwaitingOpen);

        clock.advance(config().open_transition_timeout());
        assert_eq!(s.tick(), Tick::Resolved(RecoveryOutcome::Failed));
    }

    #[test]
    fn a_late_transition_still_counts_as_success() {
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::SucceedWithoutTransition]),
            FakeMeter::silent(),
            FakeRemote::playing(),
        );

        clock.advance_secs(1);
        assert!(matches!(s.tick(), Tick::Dispatched(_)));

        // The transition arrives asynchronously, inside the window.
        clock.advance_secs(1);
        s.sink().set_link(LinkState::Opened);
        assert_eq!(s.tick(), Tick::Resolved(RecoveryOutcome::Succeeded));
    }

    #[test]
    fn an_absent_phone_resolves_as_no_remote_and_never_escalates() {
        // The always-armed steady state. Open() times out forever because the
        // phone is in another room; that must not look like a fault.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::Fail(SinkError::TimedOut)]),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        for _ in 0..200 {
            clock.advance_secs(2);
            match s.tick() {
                Tick::Dispatched(Action::ReArm(_)) | Tick::Idle => {}
                other => panic!("waiting for a phone must stay benign, got {other:?}"),
            }
        }

        assert_eq!(
            s.status(),
            HealthStatus::Listening,
            "advertising into an empty room is Listening, not Disconnected"
        );
    }

    #[test]
    fn a_powered_off_phone_resolves_as_no_remote_and_never_escalates() {
        // The measured shape of "phone Bluetooth is off": not RequestTimedOut,
        // which the design originally assumed, but UnknownFailure carrying
        // 0x8007001F, which platform/ maps to Unreachable. Before this was
        // mapped it landed in SinkError::Other and escalated - so this test is
        // guarding a bug that actually existed.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::Fail(SinkError::Unreachable)]),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        for _ in 0..200 {
            clock.advance_secs(2);
            match s.tick() {
                Tick::Dispatched(Action::ReArm(_)) | Tick::Idle => {}
                other => panic!("a powered-off phone must stay benign, got {other:?}"),
            }
        }

        assert_eq!(s.status(), HealthStatus::Listening);
    }

    #[test]
    fn a_genuine_open_failure_still_escalates() {
        // The other half of the same mapping: something that is NOT "come back
        // later" must still reach the ladder, or a real fault retries forever
        // in silence.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::Fail(SinkError::Other("denied".into()))]),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        let mut escalated = false;
        for _ in 0..50 {
            clock.advance_secs(2);
            if let Tick::Dispatched(Action::Recover(_)) = s.tick() {
                escalated = true;
                break;
            }
        }
        assert!(escalated, "a real failure must eventually reach the ladder");
    }

    #[test]
    fn only_one_open_is_in_flight_at_a_time() {
        // A trigger arriving mid-open must not start a second reconnect.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::scripted(vec![OpenBehavior::SucceedWithoutTransition]),
            FakeMeter::silent(),
            FakeRemote::playing(),
        );

        clock.advance_secs(1);
        assert!(matches!(s.tick(), Tick::Dispatched(_)));
        assert_eq!(s.sink().open_calls(), 1);

        s.note_trigger(Trigger::Manual);
        for _ in 0..10 {
            clock.advance_ms(100);
            s.tick();
        }
        assert_eq!(
            s.sink().open_calls(),
            1,
            "a trigger during an in-flight open must not open again"
        );
    }

    #[test]
    fn a_closed_link_is_rearmed_without_a_log_entry() {
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::healthy(),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        clock.advance_secs(1);
        match s.tick() {
            Tick::Dispatched(action) => {
                assert!(matches!(action, Action::ReArm(_)));
                assert!(
                    !action.is_loggable(),
                    "a benign reopen must not reach the reconnect log"
                );
            }
            other => panic!("expected a re-arm, got {other:?}"),
        }
    }

    #[test]
    fn a_paused_phone_is_never_reconnected() {
        // The measured false-positive case: link Opened, session present, peak
        // exactly zero, indefinitely - because the user pressed pause. With
        // Signal B unavailable this is indistinguishable from a dead path, so
        // the conservative default must hold and do nothing.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        for _ in 0..600 {
            clock.advance_ms(100);
            assert_eq!(s.tick(), Tick::Idle);
        }
        assert_eq!(s.sink().open_calls(), 0, "a pause is not a fault");
    }

    #[test]
    fn an_explicitly_paused_remote_is_never_reconnected() {
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::silent(),
            FakeRemote::with(Some(PlaybackStatus::Paused)),
        );

        for _ in 0..600 {
            clock.advance_ms(100);
            assert_eq!(s.tick(), Tick::Idle);
        }
        assert_eq!(s.sink().open_calls(), 0);
    }
}
