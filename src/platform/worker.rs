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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::core::{Action, Config, HealthStatus, Supervisor, SystemClock, Tick, Trigger};
use crate::platform::meter::MeterScope;
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

/// The latest view of the world, for the UI to render.
///
/// A mutex over the newest value rather than a queue: the UI redraws far
/// faster than the supervisor ticks and only ever wants the current state, so
/// a channel would just build a backlog of stale frames.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: HealthStatus,
    pub peak: f32,
    pub scope: MeterScope,
    pub device_name: String,
    /// How many of the three event-driven triggers registered. Surfaced because
    /// a missing one silently degrades recovery, and "it stopped waking up
    /// after sleep" is otherwise very hard to diagnose.
    pub triggers_registered: usize,
    /// Whether the polled default-device watch is running. Same reasoning: it
    /// can fail to construct, and a silent failure looks exactly like a device
    /// change that never happens.
    pub device_watch_active: bool,
    /// Benign re-arms dispatched so far, with the most recent reason.
    ///
    /// Deliberately NOT in `log` - the reconnect log is for genuine recoveries
    /// and filling it with an entry every time a song ends tells the user
    /// nothing. But a re-arm leaves no other trace, which made it impossible
    /// to tell a trigger that fired from one that never registered.
    pub rearms: u64,
    pub last_rearm: Option<String>,
    pub log: Vec<LogEntry>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: HealthStatus::Disconnected,
            peak: 0.0,
            scope: MeterScope::Endpoint,
            device_name: String::new(),
            triggers_registered: 0,
            device_watch_active: false,
            rearms: 0,
            last_rearm: None,
            log: Vec::new(),
        }
    }
}

/// Handle to a running supervisor thread.
pub struct Worker {
    commands: Sender<Command>,
    shared: Arc<Mutex<Snapshot>>,
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
        let running = Arc::new(AtomicBool::new(true));
        let (commands, rx) = channel();
        let (ready_tx, ready_rx) = channel();

        let thread_shared = Arc::clone(&shared);
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
        let remote = unsafe { GsmtcRemote::new() }.context("could not reach GSMTC")?;
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
    if let Ok(mut s) = shared.lock() {
        s.device_name = device_name;
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

        if let Ok(mut s) = shared.lock() {
            s.status = sup.status();
            s.peak = reading.peak;
            s.scope = reading.scope;

            if let Tick::Dispatched(action) = tick {
                if action.is_loggable() {
                    push_log(&mut s, action);
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

fn push_log(snapshot: &mut Snapshot, action: Action) {
    snapshot.log.push(LogEntry {
        at: SystemTime::now(),
        reason: format!("{action:?}"),
    });
    // Bounded, oldest first out.
    let overflow = snapshot.log.len().saturating_sub(LOG_CAPACITY);
    if overflow > 0 {
        snapshot.log.drain(0..overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ReArmReason, RecoveryReason};

    #[test]
    fn the_log_is_bounded() {
        let mut s = Snapshot::default();
        for _ in 0..(LOG_CAPACITY * 2) {
            push_log(
                &mut s,
                Action::Recover(RecoveryReason::ReArmExhausted { attempts: 1 }),
            );
        }
        assert_eq!(s.log.len(), LOG_CAPACITY, "an overnight run must not leak");
    }

    #[test]
    fn the_log_keeps_the_newest_entries() {
        let mut s = Snapshot::default();
        for i in 0..(LOG_CAPACITY + 10) {
            push_log(
                &mut s,
                Action::Recover(RecoveryReason::ReArmExhausted { attempts: i as u32 }),
            );
        }
        // "It healed itself twice in the last hour" is the question the log
        // answers, so dropping the newest would defeat the point.
        assert!(
            s.log
                .last()
                .unwrap()
                .reason
                .contains(&format!("{}", LOG_CAPACITY + 9)),
            "kept the wrong end: {:?}",
            s.log.last()
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
