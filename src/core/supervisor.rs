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
use crate::core::types::{
    Action, HealthStatus, LinkState, Observation, Presence, ReArmReason, RecoveryOutcome, Trigger,
};

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
    /// The remote's two bonds, pushed in from outside rather than read here.
    ///
    /// Not behind a trait like the other three signals, deliberately. Reading
    /// it is a WinRT round-trip per bond and it changes on the timescale of a
    /// person walking out of the room, so the owner samples it slowly and
    /// hands it over; polling it at the tick rate would cost far more than it
    /// could ever tell us.
    presence: Presence,
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
            presence: Presence::default(),
        }
    }

    pub fn status(&self) -> HealthStatus {
        self.monitor.status()
    }

    /// The configuration the state machine is running with. Callers need it to
    /// present the same thresholds the decisions are made on - a UI that drew
    /// its own silence line would eventually disagree with the watchdog.
    pub fn config(&self) -> &Config {
        self.monitor.config()
    }

    pub fn note_trigger(&mut self, trigger: Trigger) {
        self.monitor.note_trigger(trigger);
    }

    /// Update what we believe about the remote's bonds.
    ///
    /// Cheap and idempotent; the owner may call it as often or as rarely as
    /// it likes. It never itself provokes a decision.
    pub fn note_presence(&mut self, presence: Presence) {
        self.presence = presence;
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
                self.dispatch(action);
                Tick::Dispatched(action)
            }
        }
    }

    fn observe(&self) -> Observation {
        Observation {
            link: self.sink.link_state(),
            remote: self.remote.status(),
            peak: self.meter.peak(),
            presence: self.presence,
        }
    }

    /// Carry out whatever the state machine asked for.
    fn dispatch(&mut self, action: Action) {
        // The render target follows a reopen; the METER does not. It binds its
        // endpoint once at construction, so after a default-device change a
        // re-armed link would play fine while the meter read the old endpoint
        // and reported silence forever.
        let device_changed = matches!(
            action,
            Action::ReArm(ReArmReason::Trigger(Trigger::DefaultDeviceChanged))
        );
        if device_changed {
            self.meter.rebind();
        }

        // Closing is DISCARDING - on real hardware an AudioPlaybackConnection
        // cannot be reopened after a close, and reusing one returns
        // DeniedBySystem forever. So the question is when a teardown is worth
        // its cost, and the answer is: whenever the link is suspect.
        //
        // - `LinkClosed` is the routine case. The link drops constantly in
        //   ordinary use and reopening in place is measurably fine - the
        //   --rearm spike ran many arms that way without a single close.
        // - Everything else means the world changed underneath us, or the user
        //   is telling us something is wrong. A live-looking connection is
        //   exactly what is not to be trusted then.
        //
        // Manual is the one that matters most. Observed 2026-09-15: a wedged
        // link reports `Opened` with its render session `Active` and a peak of
        // zero - indistinguishable from a pause, so the watchdog cannot act,
        // which makes the Reconnect button the only remedy. It reopened the
        // dead connection in place and did nothing at all. A button that is
        // the designated fix for a state has to actually be able to fix it.
        let tears_down = match action {
            Action::Recover(_) => true,
            Action::ReArm(ReArmReason::Trigger(_)) => true,
            Action::ReArm(ReArmReason::LinkClosed) => false,
            Action::None => false,
        };
        if tears_down {
            self.sink.close();
        }
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
    fn a_default_device_change_rebinds_the_meter() {
        // The bug this guards: WasapiMeter binds its endpoint at construction
        // and does not follow a default change. Re-arming the link alone would
        // leave the meter reading the old endpoint - silent forever, while
        // audio plays perfectly out of the new one.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::with_peak(0.4),
            FakeRemote::playing(),
        );

        clock.advance_secs(1);
        assert_eq!(s.tick(), Tick::Idle);
        assert_eq!(s.meter().rebinds(), 0, "nothing happened yet");

        s.note_trigger(Trigger::DefaultDeviceChanged);
        clock.advance_secs(2);
        assert!(matches!(s.tick(), Tick::Dispatched(Action::ReArm(_))));
        assert_eq!(s.meter().rebinds(), 1);
    }

    #[test]
    fn other_triggers_leave_the_meter_alone() {
        // Rebinding drops every cached handle and the attribution flag, so
        // doing it on every trigger would throw away a resolved A2DP session
        // for no reason - and resume fires in bursts.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::with_peak(0.4),
            FakeRemote::playing(),
        );

        for trigger in [
            Trigger::Resume,
            Trigger::RadioToggled,
            Trigger::DeviceChanged,
            Trigger::Manual,
        ] {
            s.note_trigger(trigger);
            clock.advance_secs(2);
            s.tick();
            clock.advance_secs(2);
            s.tick();
        }
        assert_eq!(s.meter().rebinds(), 0);
    }

    #[test]
    fn a_manual_reconnect_tears_the_connection_down() {
        // THE bug this guards. A wedged link reports Opened with its session
        // Active and a zero peak - identical to a pause, so the watchdog is
        // correctly silent and the button is the only remedy. Reopening in
        // place does nothing to a dead connection; it has to be discarded and
        // rebuilt.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        clock.advance_secs(1);
        s.tick();
        s.note_trigger(Trigger::Manual);
        clock.advance_secs(2);
        assert!(matches!(s.tick(), Tick::Dispatched(Action::ReArm(_))));
        assert_eq!(
            s.sink().close_calls(),
            1,
            "Reconnect must discard the connection, not reopen a dead one"
        );
    }

    #[test]
    fn every_external_trigger_tears_down() {
        // A trigger means the world changed underneath us - resume, the radio
        // cycling, the device list moving. A connection that still looks live
        // is exactly what cannot be trusted at that point.
        for trigger in [
            Trigger::Manual,
            Trigger::Resume,
            Trigger::RadioToggled,
            Trigger::DeviceChanged,
            Trigger::DefaultDeviceChanged,
        ] {
            let clock = FakeClock::new();
            let mut s = sup(
                &clock,
                FakeConnection::already_open(),
                FakeMeter::with_peak(0.4),
                FakeRemote::playing(),
            );
            clock.advance_secs(1);
            s.tick();
            s.note_trigger(trigger);
            clock.advance_secs(2);
            s.tick();
            assert_eq!(
                s.sink().close_calls(),
                1,
                "{trigger:?} should tear the connection down"
            );
        }
    }

    #[test]
    fn a_benign_rearm_does_not_tear_the_connection_down() {
        // Closing is discarding: on real hardware a closed
        // AudioPlaybackConnection cannot be reopened and returns
        // DeniedBySystem forever. A re-arm happens every time the link drops
        // in ordinary use, so tearing down here would churn the radio for no
        // reason - and was an access violation before close() learned to be a
        // no-op on a never-started object.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::healthy(),
            FakeMeter::silent(),
            FakeRemote::unavailable(),
        );

        clock.advance_secs(1);
        assert!(matches!(s.tick(), Tick::Dispatched(Action::ReArm(_))));
        assert_eq!(s.sink().open_calls(), 1);
        assert_eq!(s.sink().close_calls(), 0, "a re-arm must not discard");
    }

    #[test]
    fn a_recovery_does_tear_the_connection_down() {
        // The other half: recover() means drop the connection and re-run the
        // lifecycle, which is the only way a wedged link actually comes back.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::silent(),
            FakeRemote::playing(),
        );

        // The silence condition starts on the first observation, so the
        // timeout is measured from there, not from construction.
        clock.advance_secs(1);
        assert_eq!(s.tick(), Tick::Idle);

        clock.advance_secs(7);
        assert!(matches!(s.tick(), Tick::Dispatched(Action::Recover(_))));
        assert_eq!(s.sink().close_calls(), 1);
    }

    #[test]
    fn a_default_device_change_tears_down_too() {
        // The render target is bound when the connection opens and does not
        // follow the system default, so this one needs a real reopen rather
        // than another Open() on the same object.
        let clock = FakeClock::new();
        let mut s = sup(
            &clock,
            FakeConnection::already_open(),
            FakeMeter::with_peak(0.4),
            FakeRemote::playing(),
        );

        clock.advance_secs(1);
        s.tick();
        s.note_trigger(Trigger::DefaultDeviceChanged);
        clock.advance_secs(2);
        assert!(matches!(s.tick(), Tick::Dispatched(Action::ReArm(_))));
        assert_eq!(s.sink().close_calls(), 1);
        assert_eq!(s.meter().rebinds(), 1);
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
