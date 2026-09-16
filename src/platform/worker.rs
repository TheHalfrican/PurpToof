//! The supervisor on its own thread, with a channel boundary.
//!
//! # Why this exists
//!
//! COM objects are apartment-bound. The `AudioPlaybackConnection`, the WASAPI
//! meter and the session manager all have to live on the thread that created
//! them, and that thread has to be the one that touches them. As soon as there
//! is a UI thread - milestone 8, when eframe claims the main thread and may
//! want STA - the supervisor cannot share it. Building the boundary now means
//! the UI inherits something that works instead of a refactor.
//!
//! It buys three things beyond that:
//!
//! - **The UI can never be blocked by the supervisor.** `Open()` is
//!   synchronous and takes 0.8-4.9s when the phone is unreachable. That is
//!   tolerable on a dedicated thread and unacceptable on a UI thread.
//! - **MTA, deliberately.** All four milestone-7 re-arm triggers can then be
//!   callback-based with no message pump: `RegisterSuspendResumeNotification`
//!   has a `DEVICE_NOTIFY_CALLBACK` form, `IMMNotificationClient` is a
//!   free-threaded COM callback, and the two WinRT events deliver on
//!   threadpool threads.
//! - **Trigger callbacks land next to the objects they affect.** They post a
//!   [`Command`] rather than touching COM, which is required for
//!   `IMMNotificationClient` regardless: its callbacks must not block or
//!   re-enter the enumerator.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::core::power::RadioPowerPolicy;
use crate::core::{
    Action, Config, HealthStatus, RecoveryOutcome, Supervisor, SystemClock, Tick, Trigger,
};
use crate::platform::meter::MeterScope;
use crate::platform::radio_power;
use crate::platform::triggers::{DefaultDeviceWatch, Triggers};
use crate::platform::{GsmtcRemote, Sink, WasapiMeter};

/// The poll rate the whole design assumes: fast enough to notice a dead path
/// within the silence timeout, slow enough to cost nothing.
const POLL: Duration = Duration::from_millis(100);

/// How many reconnect-log entries to keep in memory.
///
/// The log answers "did it heal itself overnight", so it wants to be long
/// enough to cover a night and short enough that an unbounded leak is
/// impossible. Durable history is the rolling file at milestone 9.
const LOG_CAPACITY: usize = 500;

/// What the UI sends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// A re-arm trigger, from a button or an OS callback. Debounced inside the
    /// state machine, so callers may send bursts freely.
    Trigger(Trigger),
    Shutdown,
}

/// One user-visible recovery. Only genuine recoveries land here - benign
/// re-arms are deliberately excluded, or the log fills with noise every time a
/// song ends and stops telling anyone anything.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: SystemTime,
    pub reason: String,
}

/// How many peak samples to retain, at the 10 Hz tick rate.
///
/// 15 seconds. Long enough to see that audio stopped a moment ago rather than
/// only that it is silent now - which matters more here than in most meters,
/// because the app provably cannot tell a pause from a dead path and the user
/// is the one making that call.
pub const PEAK_HISTORY: usize = 150;

/// The latest view of the world, for the UI to render.
///
/// A mutex over the newest value rather than a queue: the UI redraws far
/// faster than the supervisor ticks and only ever wants the current state, so
/// a channel would just build a backlog of stale frames.
///
/// Deliberately does **not** carry the reconnect log. Cloning up to
/// [`LOG_CAPACITY`] owned strings on every frame is pure waste when the log
/// changes a handful of times a day; callers watch [`Snapshot::log_len`] and
/// call [`Worker::log`] only when it moves.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: HealthStatus,
    pub peak: f32,
    pub scope: MeterScope,
    /// Newest last, at the 10 Hz tick rate, capped at [`PEAK_HISTORY`].
    pub peak_history: VecDeque<f32>,
    /// How long the meter has read below `silence_eps`, or `None` if audio is
    /// moving right now. Measured from the sample where it went quiet, not
    /// from process start.
    pub silent_for: Option<Duration>,
    pub device_name: String,
    /// How many of the three event-driven triggers registered. Surfaced because
    /// a missing one silently degrades recovery, and "it stopped waking up
    /// after sleep" is otherwise very hard to diagnose.
    pub triggers_registered: usize,
    /// Whether the polled default-device watch is running. Same reasoning: it
    /// can fail to construct, and a silent failure looks exactly like a device
    /// change that never happens.
    pub device_watch_active: bool,
    /// Whether Windows may power the Bluetooth adapter down.
    ///
    /// Resolved once at startup and after the user applies the fix. It is a
    /// static setting that only a human changes, so re-reading it at the tick
    /// rate would be a registry hit every 100ms for a value that moves twice a
    /// year.
    pub radio_power: RadioPowerPolicy,
    /// The adapter's devnode id, for the fix to act on. `None` when the
    /// adapter could not be identified, which is also why `radio_power` would
    /// read `Unknown`.
    pub adapter_devnode: Option<String>,
    /// Benign re-arms dispatched so far, with the most recent reason.
    ///
    /// Deliberately NOT in `log` - the reconnect log is for genuine recoveries
    /// and filling it with an entry every time a song ends tells the user
    /// nothing. But a re-arm leaves no other trace, which made it impossible
    /// to tell a trigger that fired from one that never registered.
    pub rearms: u64,
    pub last_rearm: Option<String>,
    /// Number of entries in the reconnect log. Watch this rather than cloning
    /// the log itself; fetch with [`Worker::log`] when it changes.
    pub log_len: usize,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: HealthStatus::Disconnected,
            peak: 0.0,
            scope: MeterScope::Endpoint,
            peak_history: VecDeque::new(),
            silent_for: None,
            device_name: String::new(),
            triggers_registered: 0,
            device_watch_active: false,
            radio_power: RadioPowerPolicy::Unknown,
            adapter_devnode: None,
            rearms: 0,
            last_rearm: None,
            log_len: 0,
        }
    }
}

/// Handle to a running supervisor thread.
pub struct Worker {
    commands: Sender<Command>,
    shared: Arc<Mutex<Snapshot>>,
    log: Arc<Mutex<Vec<LogEntry>>>,
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start the supervisor on its own MTA thread.
    ///
    /// Blocks until the thread has built its COM objects, so a failure to find
    /// a paired device is reported here rather than vanishing into a thread
    /// nobody is watching.
    pub fn spawn(config: Config) -> Result<Self> {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));
        let (commands, rx) = channel();
        let (ready_tx, ready_rx) = channel();

        let thread_shared = Arc::clone(&shared);
        let thread_log = Arc::clone(&log);
        let thread_running = Arc::clone(&running);

        let trigger_tx = commands.clone();
        let join = std::thread::Builder::new()
            .name("purptoof-supervisor".into())
            .spawn(move || {
                worker_main(
                    config,
                    rx,
                    trigger_tx,
                    thread_shared,
                    thread_log,
                    thread_running,
                    ready_tx,
                );
            })
            .context("could not spawn the supervisor thread")?;

        // Propagate a construction failure as an ordinary error.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                shared,
                log,
                running,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(anyhow!(e))
            }
            Err(_) => {
                let _ = join.join();
                Err(anyhow!("supervisor thread died before reporting readiness"))
            }
        }
    }

    /// The newest state. Cheap; call it every frame.
    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// The reconnect log. Clones owned strings, so call it only when
    /// [`Snapshot::log_len`] has changed rather than every frame.
    pub fn log(&self) -> Vec<LogEntry> {
        self.log
            .lock()
            .map(|l| l.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Post a trigger. Never blocks; a dead worker is not an error worth
    /// propagating to a button press.
    pub fn trigger(&self, trigger: Trigger) {
        let _ = self.commands.send(Command::Trigger(trigger));
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            // The worst case is one poll interval plus an in-flight Open(),
            // which is bounded at a few seconds.
            let _ = join.join();
        }
    }
}

fn worker_main(
    config: Config,
    commands: Receiver<Command>,
    trigger_tx: Sender<Command>,
    shared: Arc<Mutex<Snapshot>>,
    log: Arc<Mutex<Vec<LogEntry>>>,
    running: Arc<AtomicBool>,
    ready: Sender<std::result::Result<(), String>>,
) {
    // MTA, and on THIS thread: everything below is apartment-bound to it.
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        let _ = ready.send(Err(format!("CoInitializeEx failed: {e}")));
        running.store(false, Ordering::Relaxed);
        return;
    }

    let built = (|| -> Result<(Sink, WasapiMeter, GsmtcRemote)> {
        let sink = unsafe { Sink::connect_first() }.context("could not construct the sink")?;
        let meter = unsafe { WasapiMeter::new() }.context("could not build the meter")?;
        // The device name is what lets Signal B tell the phone's session from a
        // media player running on this PC. Without it, a browser reporting
        // Playing drives the watchdog.
        let remote =
            unsafe { GsmtcRemote::new(&sink.device().name) }.context("could not reach GSMTC")?;
        Ok((sink, meter, remote))
    })();

    let (sink, meter, remote) = match built {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(format!("{e:#}")));
            running.store(false, Ordering::Relaxed);
            unsafe { CoUninitialize() };
            return;
        }
    };

    let device_name = sink.device().name.clone();

    // Read once, here, on the COM-initialised thread. An adapter that Windows
    // is allowed to switch off sits underneath every signal the supervisor
    // watches and can take them all out at once, so the user is told even
    // though nothing in the state machine acts on it.
    let adapter_devnode = unsafe { radio_power::adapter_devnode_id() }
        .inspect_err(|e| tracing::warn!(error = %e, "could not identify the Bluetooth adapter"))
        .ok();
    let radio_power = adapter_devnode
        .as_deref()
        .map(radio_power::policy_for)
        .unwrap_or(RadioPowerPolicy::Unknown);

    if let Ok(mut s) = shared.lock() {
        s.device_name = device_name;
        s.radio_power = radio_power;
        s.adapter_devnode = adapter_devnode;
    }

    let mut sup = Supervisor::new(SystemClock, config, sink, meter, remote);

    // Registered after the supervisor exists so a trigger arriving instantly -
    // a device watcher reports every already-present device on Start - finds a
    // channel someone is draining.
    let triggers = unsafe { Triggers::register(trigger_tx) };
    let mut device_watch = match unsafe { DefaultDeviceWatch::new() } {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(error = %e, "default-device watch unavailable");
            None
        }
    };
    if let Ok(mut s) = shared.lock() {
        s.triggers_registered = triggers.registered();
        s.device_watch_active = device_watch.is_some();
    }

    let _ = ready.send(Ok(()));

    let silence_eps = sup.config().silence_eps;
    let mut silent_since: Option<Instant> = Some(Instant::now());

    // Previous values, so the rolling file records *transitions* rather than
    // the same line ten times a second. The reconnect log is in memory and
    // dies with the process; these lines are what survive a restart, which is
    // the difference between "it healed itself twice overnight" and "it has
    // been broken for an hour". Four restarts during one outage erased every
    // trace of it, which is what prompted this.
    let mut last_status: Option<HealthStatus> = None;
    let mut last_outcome: Option<RecoveryOutcome> = None;

    while let ControlFlow::Continue = drain_commands(&commands, &mut sup) {
        // Polled rather than event-driven; see DefaultDeviceWatch. Checked
        // before the tick so a change is acted on this pass rather than next.
        if let Some(watch) = &mut device_watch
            && watch.changed()
        {
            sup.note_trigger(Trigger::DefaultDeviceChanged);
        }

        let tick = sup.tick();
        let reading = sup.meter().read();

        // Tracked here rather than in the UI so the history is sampled at the
        // honest 10 Hz tick rate. A UI that built it at frame rate would
        // stretch each real sample across several pixels and imply detail the
        // meter never had.
        if reading.peak >= silence_eps {
            silent_since = None;
        } else if silent_since.is_none() {
            silent_since = Some(Instant::now());
        }

        let status = sup.status();
        if last_status != Some(status) {
            // The peak and its provenance ride along because "ConnectedSilent"
            // means something very different depending on whether the meter
            // was reading the A2DP session or falling back to the endpoint.
            tracing::info!(
                ?status,
                peak = reading.peak,
                scope = ?reading.scope,
                "status changed"
            );
            last_status = Some(status);
        }

        if let Tick::Resolved(outcome) = tick
            && last_outcome != Some(outcome)
        {
            // NoRemote is the steady state with the phone away and repeats
            // forever, so only the transition is worth a line.
            tracing::info!(?outcome, "open resolved");
            last_outcome = Some(outcome);
        }

        if let Ok(mut s) = shared.lock() {
            s.status = status;
            s.peak = reading.peak;
            s.scope = reading.scope;
            s.silent_for = silent_since.map(|t| t.elapsed());

            s.peak_history.push_back(reading.peak);
            while s.peak_history.len() > PEAK_HISTORY {
                s.peak_history.pop_front();
            }

            if let Tick::Dispatched(action) = tick {
                if action.is_loggable() {
                    if let Ok(mut l) = log.lock() {
                        push_log(&mut l, action);
                        s.log_len = l.len();
                    }
                } else {
                    s.rearms += 1;
                    s.last_rearm = Some(format!("{action:?}"));
                }
            }
        }

        std::thread::sleep(POLL);
    }

    // Unregister before the COM teardown below, so no callback can fire into
    // an apartment that is going away.
    drop(triggers);

    running.store(false, Ordering::Relaxed);
    unsafe { CoUninitialize() };
}

enum ControlFlow {
    Continue,
    Stop,
}

fn drain_commands<S, M, R, C>(
    commands: &Receiver<Command>,
    sup: &mut Supervisor<S, M, R, C>,
) -> ControlFlow
where
    S: crate::core::SinkConnection,
    M: crate::core::AudioMeter,
    R: crate::core::RemotePlayback,
    C: crate::core::Clock + Clone,
{
    loop {
        match commands.try_recv() {
            Ok(Command::Trigger(t)) => sup.note_trigger(t),
            Ok(Command::Shutdown) => return ControlFlow::Stop,
            Err(TryRecvError::Empty) => return ControlFlow::Continue,
            // The handle was dropped without a Shutdown. Same meaning.
            Err(TryRecvError::Disconnected) => return ControlFlow::Stop,
        }
    }
}

fn push_log(log: &mut Vec<LogEntry>, action: Action) {
    // Both, deliberately. The in-memory copy is what the window renders; the
    // tracing line is the only part that outlives the process, and a recovery
    // nobody can see the morning after may as well not have been recorded.
    tracing::info!(?action, "recovery");

    log.push(LogEntry {
        at: SystemTime::now(),
        reason: format!("{action:?}"),
    });
    // Bounded, oldest first out.
    let overflow = log.len().saturating_sub(LOG_CAPACITY);
    if overflow > 0 {
        log.drain(0..overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ReArmReason, RecoveryReason};

    #[test]
    fn the_log_is_bounded() {
        let mut s: Vec<LogEntry> = Vec::new();
        for _ in 0..(LOG_CAPACITY * 2) {
            push_log(
                &mut s,
                Action::Recover(RecoveryReason::ReArmExhausted { attempts: 1 }),
            );
        }
        assert_eq!(s.len(), LOG_CAPACITY, "an overnight run must not leak");
    }

    #[test]
    fn the_log_keeps_the_newest_entries() {
        let mut s: Vec<LogEntry> = Vec::new();
        for i in 0..(LOG_CAPACITY + 10) {
            push_log(
                &mut s,
                Action::Recover(RecoveryReason::ReArmExhausted { attempts: i as u32 }),
            );
        }
        // "It healed itself twice in the last hour" is the question the log
        // answers, so dropping the newest would defeat the point.
        assert!(
            s.last()
                .unwrap()
                .reason
                .contains(&format!("{}", LOG_CAPACITY + 9)),
            "kept the wrong end: {:?}",
            s.last()
        );
    }

    #[test]
    fn benign_rearms_are_never_loggable() {
        // The distinction the whole re-arm/recovery split exists for. If this
        // regresses, the log fills with an entry every time a song ends.
        assert!(!Action::ReArm(ReArmReason::LinkClosed).is_loggable());
        assert!(!Action::ReArm(ReArmReason::Trigger(Trigger::Resume)).is_loggable());
        assert!(Action::Recover(RecoveryReason::ReArmExhausted { attempts: 1 }).is_loggable());
    }
}
