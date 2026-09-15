//! The re-arm triggers: events that should re-run the lifecycle independently
//! of the meter watchdog.
//!
//! # The rule every one of these follows
//!
//! **A callback posts a [`Command::Trigger`] and does nothing else.** It never
//! touches COM, never blocks, and never calls back into the object that
//! notified it. The state machine debounces a burst into a single re-arm, so
//! callers may fire freely; resume in particular arrives as several
//! overlapping notifications.
//!
//! # Why no message pump
//!
//! These all run on the supervisor's MTA thread or on threadpool threads it
//! can be notified from, so none of them needs a window handle:
//!
//! - `RegisterSuspendResumeNotification` is used in its `DEVICE_NOTIFY_CALLBACK`
//!   form, which takes a function pointer instead of an `HWND`.
//! - `Radio::StateChanged` and `DeviceWatcher` are WinRT events, delivered on
//!   threadpool threads in MTA.
//! - The default render endpoint is **polled** rather than watched.
//!   `IMMNotificationClient` would need a COM interface implemented in Rust,
//!   which the `windows` crate cannot do without a feature this version does
//!   not ship. Comparing the endpoint id once a second costs nothing and a
//!   device change does not need sub-second latency.

use std::ffi::c_void;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use windows::Devices::Enumeration::{DeviceInformation, DeviceWatcher};
use windows::Devices::Radios::{Radio, RadioKind};
use windows::Foundation::TypedEventHandler;
use windows::Media::Audio::AudioPlaybackConnection;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, RegisterSuspendResumeNotification,
    UnregisterSuspendResumeNotification,
};
// The flag constant lives with the window-based notification APIs even though
// this is the callback form that needs no window.
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;

use crate::core::Trigger;
use crate::platform::worker::Command;

/// `PBT_APMRESUMEAUTOMATIC` - the system resumed, possibly without user
/// presence. Fires on modern standby as well as classic sleep.
const PBT_APMRESUMEAUTOMATIC: u32 = 18;
/// `PBT_APMRESUMESUSPEND` - the system resumed after a user-initiated suspend.
const PBT_APMRESUMESUSPEND: u32 = 7;

/// Every registration, held together so they are released as a unit.
///
/// Dropping this unregisters everything. That matters for the suspend/resume
/// callback in particular, which holds a raw pointer to a heap-allocated
/// [`Sender`] that must outlive the registration and be freed after it.
/// The suspend/resume registration and everything it points at.
///
/// Both allocations are boxed and held for the registration's lifetime. The
/// docs do not promise that Windows copies the subscribe parameters, and a
/// dangling `Callback`/`Context` pair would be a crash on resume - the one
/// moment the app most needs to work.
struct PowerReg {
    handle: HPOWERNOTIFY,
    /// Must keep a stable address for as long as the registration lives.
    _params: Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>,
    /// The sender the callback dereferences. Reclaimed in `Drop` *after* the
    /// registration is torn down, never before.
    context: *mut Sender<Command>,
}

pub struct Triggers {
    power: Option<PowerReg>,
    radios: Vec<(Radio, i64)>,
    watcher: Option<DeviceWatcher>,
}

// The raw pointer is only ever read by the power callback, which we unregister
// before freeing it. Nothing else touches it, and `Sender` is itself `Send`.
unsafe impl Send for Triggers {}

impl Triggers {
    /// Register everything, best effort.
    ///
    /// A trigger that fails to register is logged and skipped rather than
    /// failing the app: losing one re-arm source degrades recovery, while
    /// refusing to start removes it entirely. The meter watchdog and the
    /// always-armed sink still work without any of these.
    ///
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn register(tx: Sender<Command>) -> Self {
        let mut triggers = Self {
            power: None,
            radios: Vec::new(),
            watcher: None,
        };

        match unsafe { register_power(tx.clone()) } {
            Ok(reg) => triggers.power = Some(reg),
            Err(e) => tracing::warn!(error = %e, "resume trigger unavailable"),
        }

        match register_radios(tx.clone()) {
            Ok(radios) => triggers.radios = radios,
            Err(e) => tracing::warn!(error = %e, "radio trigger unavailable"),
        }

        match register_device_watcher(tx) {
            Ok(watcher) => triggers.watcher = Some(watcher),
            Err(e) => tracing::warn!(error = %e, "device watcher unavailable"),
        }

        triggers
    }

    /// How many registrations actually took, for the startup line.
    pub fn registered(&self) -> usize {
        self.power.is_some() as usize
            + !self.radios.is_empty() as usize
            + self.watcher.is_some() as usize
    }
}

impl Drop for Triggers {
    fn drop(&mut self) {
        if let Some(reg) = self.power.take() {
            let _ = unsafe { UnregisterSuspendResumeNotification(reg.handle) };
            // Only now is the callback guaranteed not to run again, so only
            // now is it safe to free what it dereferences. Freeing first is a
            // use-after-free on any notification already in flight.
            drop(unsafe { Box::from_raw(reg.context) });
        }

        for (radio, token) in self.radios.drain(..) {
            let _ = radio.RemoveStateChanged(token);
        }

        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.Stop();
        }
    }
}

// --- resume from sleep -------------------------------------------------------

/// The `DEVICE_NOTIFY_CALLBACK` entry point.
///
/// Runs on an OS thread with no apartment of ours, so it does exactly one
/// thing: post a command. Anything heavier here risks deadlocking the power
/// transition itself.
unsafe extern "system" fn power_callback(
    context: *const c_void,
    event: u32,
    _setting: *const c_void,
) -> u32 {
    if event != PBT_APMRESUMEAUTOMATIC && event != PBT_APMRESUMESUSPEND {
        return 0;
    }
    if context.is_null() {
        return 0;
    }
    // SAFETY: the pointer is a Box<Sender<Command>> leaked in register_power
    // and freed only after UnregisterSuspendResumeNotification has returned,
    // so it is live for as long as this callback can run.
    let tx = unsafe { &*(context as *const Sender<Command>) };
    let _ = tx.send(Command::Trigger(Trigger::Resume));
    0
}

/// # Safety
///
/// Caller must be on a COM-initialized thread.
unsafe fn register_power(tx: Sender<Command>) -> Result<PowerReg> {
    let context = Box::into_raw(Box::new(tx));
    let params = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(power_callback),
        Context: context as *mut c_void,
    });

    let handle = unsafe {
        RegisterSuspendResumeNotification(
            HANDLE(params.as_ref() as *const _ as *mut c_void),
            DEVICE_NOTIFY_CALLBACK,
        )
    };

    match handle {
        Ok(handle) => Ok(PowerReg {
            handle,
            _params: params,
            context,
        }),
        Err(e) => {
            // Reclaim rather than leaking on a failed registration.
            drop(unsafe { Box::from_raw(context) });
            Err(e).context("RegisterSuspendResumeNotification failed")
        }
    }
}

// --- bluetooth radio toggled -------------------------------------------------

fn register_radios(tx: Sender<Command>) -> Result<Vec<(Radio, i64)>> {
    let radios = Radio::GetRadiosAsync()
        .context("GetRadiosAsync dispatch failed")?
        .join()
        .context("GetRadiosAsync did not complete")?;

    let mut out = Vec::new();
    for radio in &radios {
        // Only the Bluetooth adapter matters. Watching wifi or cellular would
        // fire re-arms for changes that cannot possibly affect an A2DP link.
        if radio.Kind().ok() != Some(RadioKind::Bluetooth) {
            continue;
        }
        let tx = tx.clone();
        let token = radio
            .StateChanged(
                &TypedEventHandler::<Radio, windows::core::IInspectable>::new(move |_, _| {
                    let _ = tx.send(Command::Trigger(Trigger::RadioToggled));
                    Ok(())
                }),
            )
            .context("Radio::StateChanged registration failed")?;
        out.push((radio.clone(), token));
    }

    if out.is_empty() {
        anyhow::bail!("no Bluetooth radio found");
    }
    Ok(out)
}

// --- device appears / disappears ---------------------------------------------

fn register_device_watcher(tx: Sender<Command>) -> Result<DeviceWatcher> {
    let selector =
        AudioPlaybackConnection::GetDeviceSelector().context("GetDeviceSelector failed")?;
    let watcher = DeviceInformation::CreateWatcherAqsFilter(&selector)
        .context("CreateWatcherAqsFilter failed")?;

    let added = tx.clone();
    watcher
        .Added(&TypedEventHandler::new(move |_, _| {
            let _ = added.send(Command::Trigger(Trigger::DeviceChanged));
            Ok(())
        }))
        .context("DeviceWatcher::Added registration failed")?;

    let removed = tx;
    watcher
        .Removed(&TypedEventHandler::new(move |_, _| {
            let _ = removed.send(Command::Trigger(Trigger::DeviceChanged));
            Ok(())
        }))
        .context("DeviceWatcher::Removed registration failed")?;

    watcher.Start().context("DeviceWatcher::Start failed")?;
    Ok(watcher)
}

// --- default render endpoint changed -----------------------------------------

/// Polls the default render endpoint id, reporting when it changes.
///
/// `IMMNotificationClient` would be the event-driven way, but it needs a COM
/// interface implemented in Rust and this `windows` version ships no
/// `implement` feature. Polling an id string costs a `GetId` call and a string
/// compare, and a device change does not need sub-second latency.
///
/// This one is not merely a nicety: the render target is bound when the
/// connection opens and does not follow the system default, so without it,
/// switching outputs leaves audio going to the old device with no way back
/// short of a manual reconnect.
pub struct DefaultDeviceWatch {
    enumerator: IMMDeviceEnumerator,
    last: Option<String>,
}

impl DefaultDeviceWatch {
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread.
    pub unsafe fn new() -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("could not create IMMDeviceEnumerator")?;
        let mut watch = Self {
            enumerator,
            last: None,
        };
        // Seed it, so the first poll does not report a spurious change.
        watch.last = watch.current_id();
        Ok(watch)
    }

    /// `true` when the default render endpoint has changed since the last call.
    pub fn changed(&mut self) -> bool {
        let now = self.current_id();
        // A transient failure to read the id must not look like a change:
        // reporting one would tear the connection down and rebind the meter
        // for nothing.
        if now.is_none() {
            return false;
        }
        if now == self.last {
            return false;
        }
        self.last = now;
        true
    }

    fn current_id(&self) -> Option<String> {
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()?;
            let id = device.GetId().ok()?;
            let s = id.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as *const c_void));
            s
        }
    }
}
