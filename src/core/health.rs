//! The health state machine. This is the whole point of the app.
//!
//! # The rule
//!
//! Audio is considered dead when **the remote says it is playing and the
//! endpoint is silent for longer than the timeout.** Neither half alone is
//! ever sufficient:
//!
//! - Silence alone is a quiet passage, a pause, or a stopped track. Acting on
//!   it produces a reconnect storm during a quiet intro, which is worse than
//!   the original bug.
//! - `Opened` alone is the lie this project exists to catch. The link reports
//!   itself healthy while nothing comes out.
//!
//! # Two distinct paths out
//!
//! Driven by observed hardware behaviour: the link goes `Opened -> Closed`
//! unprompted during ordinary use.
//!
//! - **Re-arm** - benign, expected, no backoff, no reconnect-log entry. The
//!   link closed, or the world changed underneath us, and we simply reopen.
//! - **Recovery** - the watchdog fired. Audio is genuinely dead, the backoff
//!   ladder applies, and it gets a log entry the user can read.
//!
//! Collapsing these gives either a reconnect log full of noise every time a
//! song ends, or a ladder backed off to 60s during normal use that then
//! responds sluggishly to a real fault.

use std::time::Instant;

use crate::core::backoff::{Backoff, HEALTHY_RESET};
use crate::core::config::Config;
use crate::core::traits::Clock;
use crate::core::types::{
    Action, HealthStatus, LinkState, Observation, ReArmReason, RecoveryOutcome, RecoveryReason,
    Trigger,
};

pub struct HealthMonitor<C: Clock> {
    clock: C,
    config: Config,

    /// When the "remote says playing but the endpoint is silent" condition
    /// began. Cleared the moment any part of it stops holding.
    ///
    /// Tracked as a *condition start* rather than as "time since last peak",
    /// which matters: a global last-peak timestamp would make a long pause
    /// followed by pressing play look instantly overdue, and fire a spurious
    /// recovery on the first sample of a new track.
    playing_silent_since: Option<Instant>,

    /// When flow last became healthy. Feeds the ladder reset.
    healthy_since: Option<Instant>,

    backoff: Backoff,
    /// Earliest time the next *recovery* may fire. Re-arms ignore this.
    next_recovery_allowed_at: Option<Instant>,
    /// Attempt number of the recovery currently in flight, captured before
    /// the ladder advanced so the UI shows the attempt that is running.
    current_attempt: u32,

    /// Single-flight. While a recovery or re-arm is in progress, neither the
    /// watchdog nor a trigger may start another.
    in_flight: bool,
    /// Whether the in-flight operation is a real recovery rather than a
    /// benign re-arm. Only recoveries may show as `Reconnecting (attempt N)`:
    /// labelling an expected reopen that way implies a fault where there is
    /// none, and the attempt number belongs to the backoff ladder, which
    /// re-arms deliberately do not touch.
    in_flight_recovery: bool,

    last_rearm_at: Option<Instant>,
    /// Re-arms since flow was last actually observed. Escalates to a real
    /// recovery past the threshold so the ladder takes over.
    ///
    /// Reset by [`RecoveryOutcome::NoRemote`]: waiting for an absent phone is
    /// not a failing re-arm, and must never drive escalation.
    consecutive_rearms: u32,

    /// Whether the last arm completed with no remote present, i.e. we are
    /// advertising and waiting rather than repairing anything. Drives
    /// [`HealthStatus::Listening`].
    listening: bool,

    /// First trigger of a burst, plus when it arrived. Everything else in the
    /// burst folds into this one entry, which is what the debounce is.
    pending_trigger: Option<(Trigger, Instant)>,

    last_obs: Option<Observation>,
}

impl<C: Clock> HealthMonitor<C> {
    pub fn new(clock: C, config: Config) -> Self {
        Self {
            clock,
            config,
            playing_silent_since: None,
            healthy_since: None,
            backoff: Backoff::new(),
            next_recovery_allowed_at: None,
            current_attempt: 1,
            in_flight: false,
            in_flight_recovery: false,
            last_rearm_at: None,
            consecutive_rearms: 0,
            listening: false,
            pending_trigger: None,
            last_obs: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn backoff(&self) -> Backoff {
        self.backoff
    }

    pub fn is_recovering(&self) -> bool {
        self.in_flight
    }

    /// Record an external event that should re-run the lifecycle.
    ///
    /// Does not act immediately: the event is held for the debounce window so
    /// that a burst - resume in particular fires several overlapping
    /// notifications - collapses into a single re-arm. `Manual` outranks the
    /// rest, because a person pressing a button should get what they asked
    /// for rather than whatever arrived first.
    pub fn note_trigger(&mut self, trigger: Trigger) {
        let now = self.clock.now();
        match self.pending_trigger {
            Some((existing, at)) if existing != Trigger::Manual && trigger == Trigger::Manual => {
                // Keep the original arrival time so a manual press cannot be
                // delayed indefinitely by a continuing burst.
                self.pending_trigger = Some((Trigger::Manual, at));
            }
            Some(_) => {}
            None => self.pending_trigger = Some((trigger, now)),
        }
    }

    /// Feed one sample of the world and get the decision.
    pub fn observe(&mut self, obs: Observation) -> Action {
        let now = self.clock.now();
        self.last_obs = Some(obs);

        let audible = obs.peak >= self.config.silence_eps;
        let remote_available = obs.remote.is_some();

        // Whether we have grounds to believe audio should be coming out.
        //
        // With no session at all, the honest answer is "we cannot tell", and
        // the default is to make no claim - reconnecting on Signal A alone
        // thrashes during quiet passages. `recover_without_remote_signal`
        // opts into it, paired with the much longer degraded timeout.
        let expect_audio = match obs.remote {
            Some(status) => status.asserts_audio(),
            None => self.config.recover_without_remote_signal,
        };

        // Real audio proves whatever we last did actually worked.
        if audible && obs.link == LinkState::Opened {
            self.consecutive_rearms = 0;
        }

        // A live link means a remote did arrive, so we are no longer merely
        // advertising into an empty room.
        if obs.link == LinkState::Opened {
            self.listening = false;
        }

        // --- silence bookkeeping ------------------------------------------
        if obs.link == LinkState::Opened && expect_audio && !audible {
            if self.playing_silent_since.is_none() {
                self.playing_silent_since = Some(now);
            }
        } else {
            self.playing_silent_since = None;
        }

        let silent_for = self
            .playing_silent_since
            .map(|t| now.saturating_duration_since(t));
        let timeout = self.config.effective_silence_timeout(remote_available);
        let fault = silent_for.is_some_and(|d| d > timeout);

        // --- healthy streak, and the ladder reset -------------------------
        let unhealthy = obs.link != LinkState::Opened || fault;
        if unhealthy {
            self.healthy_since = None;
        } else if self.healthy_since.is_none() {
            self.healthy_since = Some(now);
        }

        if !self.backoff.is_reset()
            && let Some(since) = self.healthy_since
            && now.saturating_duration_since(since) >= HEALTHY_RESET
        {
            self.backoff.reset();
        }

        // --- single-flight -------------------------------------------------
        // Everything above is bookkeeping and still runs; only the decision
        // is suppressed, so a watchdog tick during an in-progress recovery
        // cannot start a second one.
        if self.in_flight {
            return Action::None;
        }

        // --- a debounced trigger ------------------------------------------
        if let Some((trigger, at)) = self.pending_trigger
            && now.saturating_duration_since(at) >= self.config.trigger_debounce()
        {
            self.pending_trigger = None;
            // A trigger says the world changed, not that we failed.
            return self.emit_rearm(
                ReArmReason::Trigger(trigger),
                now,
                trigger == Trigger::Manual,
            );
        }

        // --- the watchdog --------------------------------------------------
        if fault && let Some(d) = silent_for {
            return self.emit_recover(RecoveryReason::SilentWhilePlaying { silent_for: d }, now);
        }

        // --- a closed link -------------------------------------------------
        if obs.link == LinkState::Closed {
            if self.consecutive_rearms >= self.config.rearm_escalation_threshold {
                let attempts = self.consecutive_rearms;
                return self.emit_recover(RecoveryReason::ReArmExhausted { attempts }, now);
            }
            return self.emit_rearm(ReArmReason::LinkClosed, now, false);
        }

        Action::None
    }

    /// Report how the re-arm or recovery turned out.
    ///
    /// `Succeeded` must mean the link actually reached `Opened` - not merely
    /// that the open call returned success, which on real hardware happens
    /// while the state still reads `Closed`.
    pub fn recovery_finished(&mut self, outcome: RecoveryOutcome) {
        self.in_flight = false;
        self.in_flight_recovery = false;
        // Either way the silence window starts fresh: the connection has been
        // torn down and rebuilt, so a stale timestamp from before would make
        // the next sample look instantly overdue.
        self.playing_silent_since = None;

        match outcome {
            RecoveryOutcome::Failed => {
                // The ladder already advanced when the action was emitted, so
                // the spacing is in place. Nothing further to do - and
                // deliberately no reset, because a failure is not evidence of
                // health.
                self.healthy_since = None;
            }
            RecoveryOutcome::NoRemote => {
                // Armed and waiting. Not a failure, so it must not count
                // toward escalation: with the phone out of range this repeats
                // indefinitely, and escalating would drive the ladder to its
                // cap and leave us sluggish exactly when the phone returns.
                self.consecutive_rearms = 0;
                self.healthy_since = None;
                self.listening = true;
            }
            RecoveryOutcome::Succeeded => {}
        }

        if outcome != RecoveryOutcome::NoRemote {
            self.listening = false;
        }
    }

    /// The honest status line.
    pub fn status(&self) -> HealthStatus {
        // Only a genuine recovery earns the "Reconnecting (attempt N)" line.
        // A benign re-arm falls through and is described by the observation,
        // so an expected reopen looks like what it is rather than a fault.
        if self.in_flight && self.in_flight_recovery {
            return HealthStatus::Reconnecting {
                attempt: self.current_attempt,
            };
        }
        match self.last_obs {
            None if self.listening => HealthStatus::Listening,
            None => HealthStatus::Disconnected,
            Some(obs) => {
                if obs.link == LinkState::Closed {
                    // Armed and waiting is not the same as disconnected. The
                    // sink is advertising with an open call outstanding; the
                    // phone is simply elsewhere, and nothing needs repairing.
                    if self.listening {
                        HealthStatus::Listening
                    } else {
                        HealthStatus::Disconnected
                    }
                } else if obs.peak >= self.config.silence_eps {
                    // Audio is moving. Say so even when Signal B is missing -
                    // the degraded banner is a separate piece of UI, and
                    // hiding working audio behind it would be dishonest.
                    HealthStatus::Streaming
                } else if obs.remote.is_none() && !self.config.recover_without_remote_signal {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::ConnectedSilent
                }
            }
        }
    }

    // --- emission ----------------------------------------------------------

    fn emit_rearm(&mut self, reason: ReArmReason, now: Instant, bypass_interval: bool) -> Action {
        // Re-arms skip the backoff ladder by design, so this floor is the only
        // thing stopping a link that closes instantly on open from reopening
        // in a tight loop.
        if !bypass_interval
            && let Some(last) = self.last_rearm_at
            && now.saturating_duration_since(last) < self.config.rearm_min_interval()
        {
            return Action::None;
        }
        self.last_rearm_at = Some(now);
        self.consecutive_rearms += 1;
        self.in_flight = true;
        self.in_flight_recovery = false;
        Action::ReArm(reason)
    }

    fn emit_recover(&mut self, reason: RecoveryReason, now: Instant) -> Action {
        if let Some(allowed_at) = self.next_recovery_allowed_at
            && now < allowed_at
        {
            return Action::None;
        }
        self.current_attempt = self.backoff.attempt();
        self.next_recovery_allowed_at = Some(now + self.backoff.current());
        self.backoff.advance();
        self.in_flight = true;
        self.in_flight_recovery = true;
        self.healthy_since = None;
        self.playing_silent_since = None;
        self.consecutive_rearms = 0;
        Action::Recover(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fakes::FakeClock;
    use crate::core::types::PlaybackStatus;
    use std::time::Duration;

    const AUDIBLE: f32 = 0.5;
    const SILENT: f32 = 0.0;

    fn config() -> Config {
        Config::default()
    }

    fn monitor(clock: &FakeClock) -> HealthMonitor<&FakeClock> {
        HealthMonitor::new(clock, config())
    }

    fn obs(link: LinkState, remote: Option<PlaybackStatus>, peak: f32) -> Observation {
        Observation { link, remote, peak }
    }

    fn open_playing(peak: f32) -> Observation {
        obs(LinkState::Opened, Some(PlaybackStatus::Playing), peak)
    }

    // =====================================================================
    // THE anchor test
    // =====================================================================

    /// The production bug, caught. The link says `Opened`, the phone says
    /// `Playing`, and nothing is coming out. Everything else here exists to
    /// make sure this fires and nothing else does.
    #[test]
    fn opened_and_playing_but_silent_past_the_timeout_recovers() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        // Silence begins.
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);

        // Not yet overdue.
        clock.advance_ms(2_500);
        assert_eq!(
            m.observe(open_playing(SILENT)),
            Action::None,
            "must not fire before the timeout"
        );

        // Past it.
        clock.advance_ms(600);
        match m.observe(open_playing(SILENT)) {
            Action::Recover(RecoveryReason::SilentWhilePlaying { silent_for }) => {
                assert!(silent_for > Duration::from_secs(3));
            }
            other => panic!("expected a recovery, got {other:?}"),
        }
    }

    #[test]
    fn the_recovery_is_loggable_and_a_rearm_is_not() {
        // The distinction that keeps the reconnect log meaningful.
        assert!(
            Action::Recover(RecoveryReason::SilentWhilePlaying {
                silent_for: Duration::from_secs(4)
            })
            .is_loggable()
        );
        assert!(!Action::ReArm(ReArmReason::LinkClosed).is_loggable());
        assert!(!Action::None.is_loggable());
    }

    // =====================================================================
    // The decision table
    // =====================================================================

    /// Every remote state x audible/silent x before/after the timeout.
    ///
    /// Seven remote cases, not five: the six WinRT variants plus "no session
    /// at all", which is a distinct case and not the same as `Closed`.
    #[test]
    fn decision_table_is_exhaustive() {
        let remotes: [Option<PlaybackStatus>; 7] = [
            None,
            Some(PlaybackStatus::Closed),
            Some(PlaybackStatus::Opened),
            Some(PlaybackStatus::Changing),
            Some(PlaybackStatus::Stopped),
            Some(PlaybackStatus::Playing),
            Some(PlaybackStatus::Paused),
        ];

        for remote in remotes {
            for peak in [SILENT, AUDIBLE] {
                for elapsed in [Duration::from_secs(1), Duration::from_secs(10)] {
                    let clock = FakeClock::new();
                    let mut m = monitor(&clock);

                    let sample = obs(LinkState::Opened, remote, peak);
                    m.observe(sample);
                    clock.advance(elapsed);
                    let action = m.observe(sample);

                    // A recovery is correct only for the exact conjunction.
                    // `None` (no session) is excluded because the default
                    // config refuses to act without Signal B.
                    let expect_recover = remote == Some(PlaybackStatus::Playing)
                        && peak < config().silence_eps
                        && elapsed > config().silence_timeout();

                    if expect_recover {
                        assert!(
                            matches!(action, Action::Recover(_)),
                            "remote={remote:?} peak={peak} elapsed={elapsed:?} \
                             should recover, got {action:?}"
                        );
                    } else {
                        assert_eq!(
                            action,
                            Action::None,
                            "remote={remote:?} peak={peak} elapsed={elapsed:?} \
                             must do nothing"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn boundary_the_timeout_is_strictly_greater_than() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        m.observe(open_playing(SILENT));
        // Exactly the timeout is not yet past it.
        clock.advance(config().silence_timeout());
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);
        clock.advance_ms(1);
        assert!(matches!(
            m.observe(open_playing(SILENT)),
            Action::Recover(_)
        ));
    }

    #[test]
    fn peak_exactly_at_eps_counts_as_audible() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let eps = config().silence_eps;
        m.observe(open_playing(eps));
        clock.advance_secs(10);
        assert_eq!(
            m.observe(open_playing(eps)),
            Action::None,
            "eps is the audible floor, not part of silence"
        );
    }

    // =====================================================================
    // The false positives. These matter more than the positive cases.
    // =====================================================================

    #[test]
    fn false_positive_quiet_passage_in_a_song() {
        // Audio dips to silence briefly, then returns. A reconnect here is
        // worse than the bug being fixed.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        m.observe(open_playing(AUDIBLE));
        clock.advance_ms(1_000);
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);
        clock.advance_ms(1_500);
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);
        // Music comes back before the timeout.
        clock.advance_ms(400);
        assert_eq!(m.observe(open_playing(AUDIBLE)), Action::None);
        // And the silence window restarted, so a later dip is judged afresh.
        clock.advance_secs(10);
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);
    }

    #[test]
    fn false_positive_user_paused() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let paused = obs(LinkState::Opened, Some(PlaybackStatus::Paused), SILENT);
        for _ in 0..20 {
            assert_eq!(m.observe(paused), Action::None);
            clock.advance_secs(5);
        }
    }

    #[test]
    fn false_positive_remote_stopped() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let stopped = obs(LinkState::Opened, Some(PlaybackStatus::Stopped), SILENT);
        for _ in 0..20 {
            assert_eq!(m.observe(stopped), Action::None);
            clock.advance_secs(5);
        }
    }

    #[test]
    fn false_positive_changing_is_a_transient_not_a_fault() {
        // `Changing` is a real WinRT variant and must not be read as Playing.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let changing = obs(LinkState::Opened, Some(PlaybackStatus::Changing), SILENT);
        m.observe(changing);
        clock.advance_secs(30);
        assert_eq!(m.observe(changing), Action::None);
    }

    #[test]
    fn false_positive_no_avrcp_metadata_never_auto_recovers() {
        // Signal B unavailable. Silence forever must not trigger anything by
        // itself - the default refuses to act on Signal A alone.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let no_session = obs(LinkState::Opened, None, SILENT);
        for _ in 0..40 {
            assert_eq!(m.observe(no_session), Action::None);
            clock.advance_secs(10);
        }
        assert_eq!(m.status(), HealthStatus::Degraded, "and it says so");
    }

    #[test]
    fn false_positive_long_pause_then_play_does_not_fire_immediately() {
        // The trap a "time since last peak" design falls into: after a long
        // pause, the first sample of a new track would look overdue.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        let paused = obs(LinkState::Opened, Some(PlaybackStatus::Paused), SILENT);
        m.observe(paused);
        clock.advance_secs(600); // ten minutes paused

        // Playback resumes; the very first sample is still silent.
        assert_eq!(
            m.observe(open_playing(SILENT)),
            Action::None,
            "the silence window must start when Playing began, not at the last peak"
        );
        clock.advance_ms(500);
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);
    }

    #[test]
    fn degraded_mode_can_be_opted_into_and_uses_the_longer_timeout() {
        let clock = FakeClock::new();
        let mut cfg = config();
        cfg.recover_without_remote_signal = true;
        let mut m = HealthMonitor::new(&clock, cfg.clone());

        let no_session = obs(LinkState::Opened, None, SILENT);
        m.observe(no_session);

        // Past the normal timeout but well inside the degraded one.
        clock.advance(cfg.silence_timeout() + Duration::from_secs(1));
        assert_eq!(
            m.observe(no_session),
            Action::None,
            "the degraded timeout is more patient"
        );

        clock.advance(cfg.degraded_silence_timeout());
        assert!(
            matches!(m.observe(no_session), Action::Recover(_)),
            "but it does eventually fire when opted in"
        );
    }

    // =====================================================================
    // Re-arm vs recovery
    // =====================================================================

    #[test]
    fn a_closed_link_rearms_rather_than_recovering() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Paused), SILENT);
        assert_eq!(
            m.observe(closed),
            Action::ReArm(ReArmReason::LinkClosed),
            "an unprompted close is expected behaviour, not a fault"
        );
    }

    #[test]
    fn rearms_are_rate_limited_so_they_cannot_busy_loop() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Paused), SILENT);

        assert!(matches!(m.observe(closed), Action::ReArm(_)));
        m.recovery_finished(RecoveryOutcome::Failed);

        // Immediately again: suppressed.
        assert_eq!(m.observe(closed), Action::None);
        clock.advance_ms(100);
        assert_eq!(m.observe(closed), Action::None);

        // Past the floor: allowed.
        clock.advance_ms(1_000);
        assert!(matches!(m.observe(closed), Action::ReArm(_)));
    }

    // --- armed and waiting is not a fault -----------------------------------
    //
    // The sink is meant to stay permanently armed, so with the phone out of
    // range `Open()` returns `RequestTimedOut` indefinitely. Treating that as
    // a failing re-arm escalates to a recovery, drives the ladder to its 60s
    // cap, and leaves the app sluggish at the exact moment the phone returns.

    #[test]
    fn waiting_for_an_absent_remote_never_escalates() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, None, SILENT);

        // Far past the escalation threshold. A phone in another room for an
        // hour must never look like a fault.
        for i in 0..(config().rearm_escalation_threshold * 20) {
            clock.advance_secs(2);
            let action = m.observe(closed);
            assert!(
                matches!(action, Action::ReArm(_)),
                "arm {i} should stay a benign re-arm, got {action:?}"
            );
            m.recovery_finished(RecoveryOutcome::NoRemote);
        }

        assert!(
            m.backoff().is_reset(),
            "the ladder must never advance while merely waiting for a remote"
        );
    }

    #[test]
    fn no_remote_reports_listening_rather_than_disconnected() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        clock.advance_secs(2);
        m.observe(obs(LinkState::Closed, None, SILENT));
        m.recovery_finished(RecoveryOutcome::NoRemote);
        clock.advance_secs(2);
        m.observe(obs(LinkState::Closed, None, SILENT));

        assert_eq!(
            m.status(),
            HealthStatus::Listening,
            "advertising into an empty room is not the same as Disconnected"
        );
    }

    #[test]
    fn a_live_link_clears_listening() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        clock.advance_secs(2);
        m.observe(obs(LinkState::Closed, None, SILENT));
        m.recovery_finished(RecoveryOutcome::NoRemote);
        assert_eq!(m.status(), HealthStatus::Listening);

        clock.advance_secs(2);
        m.observe(obs(
            LinkState::Opened,
            Some(PlaybackStatus::Playing),
            AUDIBLE,
        ));
        assert_eq!(m.status(), HealthStatus::Streaming);
    }

    #[test]
    fn a_real_failure_after_waiting_still_escalates() {
        // NoRemote resets the counter, but it must not make the app immune to
        // genuine failures afterwards - a phone that connects and then cannot
        // hold a link is still a fault.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Playing), SILENT);

        for _ in 0..3 {
            clock.advance_secs(2);
            m.observe(closed);
            m.recovery_finished(RecoveryOutcome::NoRemote);
        }

        let threshold = config().rearm_escalation_threshold;
        for i in 0..threshold {
            clock.advance_secs(2);
            let action = m.observe(closed);
            assert!(
                matches!(action, Action::ReArm(_)),
                "re-arm {i} should still be benign, got {action:?}"
            );
            m.recovery_finished(RecoveryOutcome::Failed);
        }

        clock.advance_secs(2);
        match m.observe(closed) {
            Action::Recover(RecoveryReason::ReArmExhausted { attempts }) => {
                assert_eq!(attempts, threshold);
            }
            other => panic!("expected escalation after real failures, got {other:?}"),
        }
    }

    #[test]
    fn waiting_interleaved_with_failures_does_not_mask_them() {
        // The pathological ordering: a NoRemote between every failure would
        // reset the counter forever and make escalation unreachable. That is
        // correct - each NoRemote is evidence the phone genuinely was not
        // there - so assert the behaviour explicitly rather than leave it to
        // be discovered.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Playing), SILENT);

        for _ in 0..(config().rearm_escalation_threshold * 3) {
            clock.advance_secs(2);
            assert!(matches!(m.observe(closed), Action::ReArm(_)));
            m.recovery_finished(RecoveryOutcome::Failed);
            clock.advance_secs(2);
            assert!(matches!(m.observe(closed), Action::ReArm(_)));
            m.recovery_finished(RecoveryOutcome::NoRemote);
        }

        assert!(
            m.backoff().is_reset(),
            "alternating with NoRemote keeps the ladder reset by design"
        );
    }

    #[test]
    fn repeated_rearms_without_flow_escalate_to_a_logged_recovery() {
        // Re-arms skip the ladder, so something has to stop us reopening
        // forever when the link will not stay up.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Playing), SILENT);

        let threshold = config().rearm_escalation_threshold;
        for i in 0..threshold {
            clock.advance_secs(2);
            let action = m.observe(closed);
            assert!(
                matches!(action, Action::ReArm(_)),
                "re-arm {i} should still be benign, got {action:?}"
            );
            m.recovery_finished(RecoveryOutcome::Failed);
        }

        clock.advance_secs(2);
        match m.observe(closed) {
            Action::Recover(RecoveryReason::ReArmExhausted { attempts }) => {
                assert_eq!(attempts, threshold);
            }
            other => panic!("expected escalation to a recovery, got {other:?}"),
        }
    }

    #[test]
    fn observed_flow_resets_the_escalation_counter() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Playing), SILENT);

        for _ in 0..3 {
            clock.advance_secs(2);
            assert!(matches!(m.observe(closed), Action::ReArm(_)));
            m.recovery_finished(RecoveryOutcome::Succeeded);
        }

        // Real audio proves the reopen worked.
        clock.advance_secs(1);
        m.observe(open_playing(AUDIBLE));

        // The counter restarted, so we get the full allowance of benign
        // re-arms again rather than escalating early.
        for i in 0..config().rearm_escalation_threshold {
            clock.advance_secs(2);
            let action = m.observe(closed);
            assert!(
                matches!(action, Action::ReArm(_)),
                "re-arm {i} after healthy flow should be benign, got {action:?}"
            );
            m.recovery_finished(RecoveryOutcome::Failed);
        }
    }

    // =====================================================================
    // Single-flight
    // =====================================================================

    #[test]
    fn single_flight_watchdog_during_an_in_progress_recovery_does_nothing() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        m.observe(open_playing(SILENT));
        clock.advance_secs(4);
        assert!(matches!(
            m.observe(open_playing(SILENT)),
            Action::Recover(_)
        ));
        assert!(m.is_recovering());

        // The watchdog keeps ticking while the recovery runs.
        for _ in 0..10 {
            clock.advance_secs(5);
            assert_eq!(
                m.observe(open_playing(SILENT)),
                Action::None,
                "exactly one reconnect may be in flight"
            );
        }

        m.recovery_finished(RecoveryOutcome::Succeeded);
        assert!(!m.is_recovering());
    }

    #[test]
    fn single_flight_a_trigger_during_a_recovery_does_not_start_a_second() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        m.observe(open_playing(SILENT));
        clock.advance_secs(4);
        assert!(matches!(
            m.observe(open_playing(SILENT)),
            Action::Recover(_)
        ));

        m.note_trigger(Trigger::Resume);
        clock.advance_secs(2);
        assert_eq!(m.observe(open_playing(SILENT)), Action::None);

        // Once finished, the held trigger is honoured rather than lost.
        m.recovery_finished(RecoveryOutcome::Succeeded);
        assert!(matches!(
            m.observe(open_playing(AUDIBLE)),
            Action::ReArm(ReArmReason::Trigger(Trigger::Resume))
        ));
    }

    // =====================================================================
    // Debounce
    // =====================================================================

    #[test]
    fn a_burst_of_resume_events_collapses_to_one_rearm() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let healthy = open_playing(AUDIBLE);

        // Resume fires a burst of overlapping notifications.
        for _ in 0..8 {
            m.note_trigger(Trigger::Resume);
            clock.advance_ms(50);
            assert_eq!(
                m.observe(healthy),
                Action::None,
                "nothing fires inside the debounce window"
            );
        }

        clock.advance(config().trigger_debounce());
        let mut rearms = 0;
        for _ in 0..10 {
            if matches!(m.observe(healthy), Action::ReArm(_)) {
                rearms += 1;
                m.recovery_finished(RecoveryOutcome::Succeeded);
            }
            clock.advance_ms(100);
        }
        assert_eq!(rearms, 1, "the whole burst is one recovery");
    }

    #[test]
    fn a_manual_press_outranks_a_burst_it_arrives_during() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        m.note_trigger(Trigger::Resume);
        m.note_trigger(Trigger::DefaultDeviceChanged);
        m.note_trigger(Trigger::Manual);

        clock.advance(config().trigger_debounce());
        assert_eq!(
            m.observe(open_playing(AUDIBLE)),
            Action::ReArm(ReArmReason::Trigger(Trigger::Manual)),
            "the user's explicit request is what gets reported"
        );
    }

    #[test]
    fn manual_bypasses_the_rearm_floor() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Paused), SILENT);

        assert!(matches!(m.observe(closed), Action::ReArm(_)));
        m.recovery_finished(RecoveryOutcome::Failed);

        // Well inside the floor, so an automatic re-arm would be suppressed.
        clock.advance_ms(50);
        assert_eq!(m.observe(closed), Action::None);

        m.note_trigger(Trigger::Manual);
        clock.advance(config().trigger_debounce());
        assert!(
            matches!(
                m.observe(closed),
                Action::ReArm(ReArmReason::Trigger(Trigger::Manual))
            ),
            "the Reconnect button is always enabled and must always respond"
        );
    }

    #[test]
    fn each_trigger_kind_can_drive_a_rearm() {
        for trigger in [
            Trigger::Resume,
            Trigger::RadioToggled,
            Trigger::DefaultDeviceChanged,
            Trigger::DeviceChanged,
            Trigger::Manual,
        ] {
            let clock = FakeClock::new();
            let mut m = monitor(&clock);
            m.note_trigger(trigger);
            clock.advance(config().trigger_debounce());
            assert_eq!(
                m.observe(open_playing(AUDIBLE)),
                Action::ReArm(ReArmReason::Trigger(trigger)),
                "{trigger:?} should re-arm"
            );
        }
    }

    // =====================================================================
    // Backoff integration
    // =====================================================================

    #[test]
    fn consecutive_failed_recoveries_walk_the_ladder() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let dead = open_playing(SILENT);

        let expected = [1u64, 2, 5, 10, 30, 60, 60];
        let mut waits = Vec::new();

        // First fault.
        m.observe(dead);
        clock.advance_secs(4);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        m.recovery_finished(RecoveryOutcome::Failed);

        for rung in expected {
            // Re-establish the silence condition and go past the timeout.
            m.observe(dead);
            clock.advance_secs(4);

            // Step forward a second at a time until the ladder lets us fire.
            let mut waited = 0u64;
            loop {
                match m.observe(dead) {
                    Action::Recover(_) => break,
                    Action::None => {
                        clock.advance_secs(1);
                        waited += 1;
                        assert!(waited < 200, "ladder never released");
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            waits.push((rung, waited));
            m.recovery_finished(RecoveryOutcome::Failed);
        }

        // Each wait must be bounded by its rung. The 4s spent re-establishing
        // silence already covers the shorter rungs, so assert the ceiling
        // rather than an exact equality.
        for (rung, waited) in waits {
            assert!(
                waited <= rung,
                "waited {waited}s for a {rung}s rung - backoff is too slow"
            );
        }
    }

    #[test]
    fn the_ladder_resets_only_after_sustained_healthy_flow() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let dead = open_playing(SILENT);

        m.observe(dead);
        clock.advance_secs(4);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        m.recovery_finished(RecoveryOutcome::Failed);
        assert!(!m.backoff().is_reset(), "the ladder advanced");

        // A single good sample is NOT enough.
        m.observe(open_playing(AUDIBLE));
        assert!(
            !m.backoff().is_reset(),
            "one non-silent reading during a flapping connection proves nothing"
        );

        // Nor is almost-enough.
        clock.advance(HEALTHY_RESET - Duration::from_secs(1));
        m.observe(open_playing(AUDIBLE));
        assert!(!m.backoff().is_reset());

        // Sustained flow does it.
        clock.advance_secs(2);
        m.observe(open_playing(AUDIBLE));
        assert!(
            m.backoff().is_reset(),
            "60s of continuous healthy flow resets the ladder"
        );
    }

    #[test]
    fn a_fault_partway_through_the_healthy_window_does_not_reset() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let dead = open_playing(SILENT);

        m.observe(dead);
        clock.advance_secs(4);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        m.recovery_finished(RecoveryOutcome::Failed);

        // Healthy for a while...
        clock.advance_secs(40);
        m.observe(open_playing(AUDIBLE));
        // ...then the link drops, restarting the clock on health.
        clock.advance_secs(1);
        m.observe(obs(
            LinkState::Closed,
            Some(PlaybackStatus::Playing),
            SILENT,
        ));
        m.recovery_finished(RecoveryOutcome::Failed);

        clock.advance_secs(30);
        m.observe(open_playing(AUDIBLE));
        assert!(
            !m.backoff().is_reset(),
            "the healthy streak restarted when the link dropped"
        );
    }

    // =====================================================================
    // Status line
    // =====================================================================

    #[test]
    fn status_reports_what_a_user_would_recognise() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        assert_eq!(m.status(), HealthStatus::Disconnected, "before any sample");

        m.observe(open_playing(AUDIBLE));
        assert_eq!(m.status(), HealthStatus::Streaming);

        m.observe(open_playing(SILENT));
        assert_eq!(m.status(), HealthStatus::ConnectedSilent);

        m.observe(obs(
            LinkState::Closed,
            Some(PlaybackStatus::Playing),
            SILENT,
        ));
        assert_eq!(m.status(), HealthStatus::Disconnected);
    }

    #[test]
    fn status_shows_the_attempt_number_that_is_actually_running() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        let dead = open_playing(SILENT);

        m.observe(dead);
        clock.advance_secs(4);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        assert_eq!(
            m.status(),
            HealthStatus::Reconnecting { attempt: 1 },
            "the first attempt is attempt 1, not 2"
        );
        m.recovery_finished(RecoveryOutcome::Failed);

        m.observe(dead);
        clock.advance_secs(10);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        assert_eq!(m.status(), HealthStatus::Reconnecting { attempt: 2 });
    }

    #[test]
    fn a_benign_rearm_is_not_reported_as_reconnecting() {
        // The status line is the app's honesty budget. Labelling an expected
        // reopen "Reconnecting (attempt 1)" invents a fault, and the attempt
        // number is meaningless for a path that never touches the ladder.
        let clock = FakeClock::new();
        let mut m = monitor(&clock);

        let closed = obs(LinkState::Closed, Some(PlaybackStatus::Playing), SILENT);
        assert_eq!(m.observe(closed), Action::ReArm(ReArmReason::LinkClosed));
        assert_eq!(
            m.status(),
            HealthStatus::Disconnected,
            "a re-arm in flight is still just disconnected"
        );

        // A real recovery does earn the label.
        m.recovery_finished(RecoveryOutcome::Succeeded);
        let dead = open_playing(SILENT);
        m.observe(dead);
        clock.advance_secs(4);
        assert!(matches!(m.observe(dead), Action::Recover(_)));
        assert_eq!(m.status(), HealthStatus::Reconnecting { attempt: 1 });
    }

    #[test]
    fn streaming_beats_degraded_when_audio_is_genuinely_flowing() {
        let clock = FakeClock::new();
        let mut m = monitor(&clock);
        // No Signal B, but sound is coming out. Hiding that behind a degraded
        // label would misreport a working system.
        m.observe(obs(LinkState::Opened, None, AUDIBLE));
        assert_eq!(m.status(), HealthStatus::Streaming);
    }

    // =====================================================================
    // Property-based invariants
    // =====================================================================

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn any_status() -> impl Strategy<Value = Option<PlaybackStatus>> {
            prop_oneof![
                Just(None),
                Just(Some(PlaybackStatus::Closed)),
                Just(Some(PlaybackStatus::Opened)),
                Just(Some(PlaybackStatus::Changing)),
                Just(Some(PlaybackStatus::Stopped)),
                Just(Some(PlaybackStatus::Playing)),
                Just(Some(PlaybackStatus::Paused)),
            ]
        }

        fn any_link() -> impl Strategy<Value = LinkState> {
            prop_oneof![Just(LinkState::Closed), Just(LinkState::Opened)]
        }

        prop_compose! {
            fn any_step()(
                link in any_link(),
                remote in any_status(),
                peak in prop_oneof![Just(0.0f32), Just(0.5f32), 0.0f32..1.0],
                gap_ms in 0u64..4_000,
            ) -> (Observation, u64) {
                (Observation { link, remote, peak }, gap_ms)
            }
        }

        proptest! {
            /// No input sequence, however adversarial, may produce a
            /// reconnect storm. This is the invariant that protects the user
            /// from a bug in the decision logic: worse than failing to
            /// recover is hammering the radio forever.
            #[test]
            fn never_storms_regardless_of_input(steps in prop::collection::vec(any_step(), 1..300)) {
                let clock = FakeClock::new();
                let mut m = monitor(&clock);

                let window = Duration::from_secs(10);
                // The ladder's shortest rungs are 1s and 2s, and re-arms are
                // floored at 1s, so a 10s window cannot legitimately hold
                // many actions.
                let max_in_window = 11;

                let mut fired: Vec<Instant> = Vec::new();

                for (sample, gap_ms) in steps {
                    clock.advance_ms(gap_ms);
                    let action = m.observe(sample);
                    if !action.is_none() {
                        let now = clock.now();
                        fired.push(now);
                        // Close the loop the way the real app does, so the
                        // machine is never left permanently in flight.
                        m.recovery_finished(RecoveryOutcome::Failed);

                        // checked_sub because the window can reach back past
                        // the fake clock's origin on the first few actions.
                        let recent = match now.checked_sub(window) {
                            Some(cutoff) => fired.iter().filter(|t| **t >= cutoff).count(),
                            None => fired.len(),
                        };
                        prop_assert!(
                            recent <= max_in_window,
                            "{recent} actions within {window:?} is a storm"
                        );
                    }
                }
            }

            /// The other half: sustained Playing plus silence must always
            /// eventually produce a recovery. A state machine that never
            /// storms by never acting would pass the test above.
            #[test]
            fn sustained_silence_while_playing_always_recovers(
                initial_gap_ms in 0u64..2_000,
            ) {
                let clock = FakeClock::new();
                let mut m = monitor(&clock);
                let dead = open_playing(0.0);

                clock.advance_ms(initial_gap_ms);

                let mut recovered = false;
                for _ in 0..100 {
                    if matches!(m.observe(dead), Action::Recover(_)) {
                        recovered = true;
                        break;
                    }
                    clock.advance_ms(500);
                }
                prop_assert!(
                    recovered,
                    "sustained Playing + silence must eventually be recovered"
                );
            }

            /// Silence while the remote is definitively not playing must
            /// never, over any duration, produce an action while the link is
            /// open. This is the reconnect-storm-during-a-quiet-intro case.
            #[test]
            fn never_acts_on_silence_alone_while_paused_or_stopped(
                gaps in prop::collection::vec(0u64..5_000, 1..80),
                paused in any::<bool>(),
            ) {
                let clock = FakeClock::new();
                let mut m = monitor(&clock);
                let status = if paused { PlaybackStatus::Paused } else { PlaybackStatus::Stopped };
                let sample = Observation {
                    link: LinkState::Opened,
                    remote: Some(status),
                    peak: 0.0,
                };

                for gap in gaps {
                    clock.advance_ms(gap);
                    prop_assert_eq!(m.observe(sample), Action::None);
                }
            }
        }
    }
}
