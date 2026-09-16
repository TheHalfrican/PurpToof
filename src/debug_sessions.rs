//! `--debug-sessions`: a read-only dump of what Windows actually believes,
//! at every layer that the phrase "connected" gets applied to.
//!
//! This exists to resolve the two VERIFY items in CLAUDE.md, and to answer a
//! question that comes up constantly in practice: Windows says the phone is
//! connected, the phone disagrees, and neither is lying - they are reporting
//! different things.
//!
//! Five layers, reported separately and never conflated:
//!
//!   0. **The adapter.** May Windows power the radio down? This sits
//!      underneath every layer below and can take them all out at once,
//!      while none of them can see why.
//!   1. **Pairing / interface.** Does the A2DP sink interface enumerate at
//!      all? This is what makes the device eligible, nothing more.
//!   2. **The sink object.** `AudioPlaybackConnection::State()`. This was
//!      documented here as reflecting only the object *we* hold; that is
//!      **wrong**, corrected 2026-09-16. A freshly constructed object that
//!      was never opened read `Opened` while a separate PurpToof process
//!      held the link. `State()` reports whether *anyone* has the device
//!      open, so `Closed` means nobody does - not "we have not opened it".
//!   3. **Signal A - is sound actually coming out.** The default render
//!      endpoint's peak meter, plus every audio session on that endpoint with
//!      its owning process, so we can see whether A2DP render audio is
//!      attributable to a PID or lands on the audio engine.
//!   4. **Signal B - does the remote think it is playing.** GSMTC sessions,
//!      which reach us over AVRCP. This is as close to "proof from the phone"
//!      as Windows can give us.
//!
//! Everything here is read-only. It never advertises a sink, never opens a
//! stream, and never changes a device. It is safe to run on a machine in use.
//!
//! The attribution rule now lives in `platform/a2dp_session.rs`, shared with
//! the real `AudioMeter`, so the diagnostic and the thing it diagnoses cannot
//! drift apart.

use anyhow::{Context, Result};

// The attribution rule lives in platform/ so the diagnostic and the real
// AudioMeter cannot drift apart.
use purptoof::core::power::RadioPowerPolicy;
use purptoof::platform::a2dp_session::{
    looks_like_a2dp_session, session_identifier, session_instance_identifier,
};
use purptoof::platform::radio_power;
use windows::Devices::Enumeration::DeviceInformation;
use windows::Media::Audio::{AudioPlaybackConnection, AudioPlaybackConnectionState};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Variant::VT_LPWSTR;
// `Interface` brings the QueryInterface-backed `cast()` into scope.
use windows::core::{Interface, PWSTR};

/// How long to watch the meters. Long enough to distinguish "silent" from
/// "between samples", short enough not to be tedious.
const SAMPLE_SECS: u32 = 6;

pub fn run() -> Result<()> {
    println!("PurpToof --debug-sessions (read-only)\n");

    report_radio_power()?;
    report_pairing()?;
    report_our_sink()?;
    report_remote_playback()?;
    report_audio_sessions()?;

    println!(
        "\nReading this: layers 1 and 2 are LINK state and mean nothing about\n\
         whether audio is moving. Only layer 3 (a moving peak) proves sound is\n\
         coming out, and only layer 4 tells us whether the remote believes it is\n\
         playing. The watchdog fires on the CONJUNCTION - remote says Playing and\n\
         the endpoint is silent - never on either alone."
    );
    Ok(())
}

// --- Layer 0: the adapter itself -------------------------------------------

/// Whether Windows is allowed to power the radio down.
///
/// Not a "layer" in the connection sense - it sits underneath all of them,
/// because an adapter Windows may switch off can take every layer above it
/// down at once, and none of those layers can see why.
fn report_radio_power() -> Result<()> {
    println!("== layer 0: may Windows power the adapter down? ==");

    let devnode = match unsafe { radio_power::adapter_devnode_id() } {
        Ok(d) => d,
        Err(e) => {
            println!("  could not identify the adapter: {e}");
            println!("  -> reported as Unknown, which never raises the warning.");
            return Ok(());
        }
    };
    println!("  devnode: {devnode}");

    match radio_power::policy_for(&devnode) {
        RadioPowerPolicy::MayPowerDown => {
            println!("  IdleInWorkingState is set -> WINDOWS MAY POWER THIS DOWN.");
            println!("     CLAUDE.md names this as a cause of this symptom class.");
        }
        RadioPowerPolicy::HeldOn => {
            println!("  IdleInWorkingState = 0 -> the radio is held on. Good.");
        }
        RadioPowerPolicy::Unknown => {
            println!("  IdleInWorkingState absent -> unknown; no claim made.");
            println!("     The driver default applies, and this project has not");
            println!("     established what that default is.");
        }
    }
    println!();
    Ok(())
}

// --- Layer 1 ---------------------------------------------------------------

fn report_pairing() -> Result<()> {
    println!("== layer 1: pairing / A2DP sink interface ==");

    let selector =
        AudioPlaybackConnection::GetDeviceSelector().context("GetDeviceSelector failed")?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .context("FindAllAsyncAqsFilter dispatch failed")?
        .join()
        .context("FindAllAsyncAqsFilter did not complete")?;

    let count = devices.Size().unwrap_or(0);
    if count == 0 {
        println!("  (none) - nothing is paired as an A2DP source, or the interface is disabled");
        return Ok(());
    }

    for device in &devices {
        let name = device.Name().unwrap_or_default();
        let id = device.Id().unwrap_or_default();
        println!("  {name}");
        println!("    id: {id}");
        println!(
            "    NOTE: enumerating here only means paired and interface-enabled.\n\
             \x20         It does not mean a link is up or audio is flowing."
        );
    }
    Ok(())
}

// --- Layer 2 ---------------------------------------------------------------

fn state_name(s: AudioPlaybackConnectionState) -> String {
    if s == AudioPlaybackConnectionState::Closed {
        "Closed".into()
    } else if s == AudioPlaybackConnectionState::Opened {
        "Opened".into()
    } else {
        format!("<unknown {}>", s.0)
    }
}

fn report_our_sink() -> Result<()> {
    println!("\n== layer 2: our own AudioPlaybackConnection ==");

    let selector =
        AudioPlaybackConnection::GetDeviceSelector().context("GetDeviceSelector failed")?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .context("FindAllAsyncAqsFilter dispatch failed")?
        .join()
        .context("FindAllAsyncAqsFilter did not complete")?;

    if devices.Size().unwrap_or(0) == 0 {
        println!("  (no device to construct against)");
        return Ok(());
    }

    let device = devices.GetAt(0).context("device 0 vanished")?;
    let id = device.Id().context("device has no Id")?;

    match AudioPlaybackConnection::TryCreateFromId(&id) {
        Ok(c) => {
            let state = c
                .State()
                .map(state_name)
                .unwrap_or_else(|e| format!("<{e}>"));
            println!("  constructed OK, state: {state}");
            println!(
                "    NOTE: this object was constructed here and never opened, but\n\
                 \x20         `State()` is NOT scoped to it. Measured 2026-09-16: it\n\
                 \x20         read `Opened` while a separate PurpToof process held the\n\
                 \x20         link. So `Opened` means someone has this device open, and\n\
                 \x20         `Closed` means nobody does - neither is a statement about\n\
                 \x20         this object. Either way it says nothing about what Windows\n\
                 \x20         Settings shows: Settings reports the Bluetooth link\n\
                 \x20         (hands-free, AVRCP, phonebook), not this stream."
            );
            // Never advertised, so nothing to close.
            drop(c);
        }
        Err(e) => println!("  TryCreateFromId failed: {e}"),
    }
    Ok(())
}

// --- Layer 4 (reported before 3 because it is the quick one) ---------------

fn playback_status_name(s: PlaybackStatus) -> String {
    // windows-rs generates these as associated consts on a tuple struct, so
    // Debug prints a bare integer and pattern-matching on them is fragile.
    if s == PlaybackStatus::Closed {
        "Closed".into()
    } else if s == PlaybackStatus::Opened {
        "Opened".into()
    } else if s == PlaybackStatus::Changing {
        "Changing".into()
    } else if s == PlaybackStatus::Stopped {
        "Stopped".into()
    } else if s == PlaybackStatus::Playing {
        "Playing".into()
    } else if s == PlaybackStatus::Paused {
        "Paused".into()
    } else {
        format!("<unknown {}>", s.0)
    }
}

fn report_remote_playback() -> Result<()> {
    println!("\n== layer 4: Signal B - GSMTC / what the remote says (over AVRCP) ==");

    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .context("GSMTC RequestAsync dispatch failed")?
        .join()
        .context("GSMTC RequestAsync did not complete")?;

    let sessions = manager.GetSessions().context("GetSessions failed")?;
    let count = sessions.Size().unwrap_or(0);

    if count == 0 {
        println!(
            "  (no sessions at all)\n\
             \x20 Signal B is UNAVAILABLE right now. If this holds while the phone is\n\
             \x20 genuinely streaming, the app must degrade: longer silence timeout,\n\
             \x20 explicit user-triggered reconnect, and say so in the UI. Never\n\
             \x20 reconnect on Signal A alone - that thrashes on quiet passages."
        );
        return Ok(());
    }

    println!("  {count} session(s):");
    for session in &sessions {
        let app = session.SourceAppUserModelId().unwrap_or_default();
        let status = session
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .map(playback_status_name)
            .unwrap_or_else(|e| format!("<{e}>"));

        println!("    - {app}");
        println!("        PlaybackStatus: {status}");

        // Media properties are best-effort: plenty of sources publish a
        // session but no metadata, which is exactly the degraded case the
        // health model has to tolerate.
        match session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.join())
        {
            Ok(props) => {
                let title = props.Title().unwrap_or_default();
                let artist = props.Artist().unwrap_or_default();
                if title.is_empty() && artist.is_empty() {
                    println!("        metadata: (session exists but publishes none)");
                } else {
                    println!("        metadata: {title} - {artist}");
                }
            }
            Err(e) => println!("        metadata: unavailable ({e})"),
        }
    }
    Ok(())
}

// --- Layer 3 ---------------------------------------------------------------

/// Read `PKEY_Device_FriendlyName` from an endpoint's property store.
///
/// # COM memory
///
/// `GetValue` fills a `PROPVARIANT` owning a `CoTaskMemAlloc`'d string. The
/// `windows` crate's `PROPVARIANT` is the raw ABI struct with no `Drop`, so the
/// value is copied out *before* `PropVariantClear` releases it - reading after
/// the clear would be a use-after-free.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread and `device` must be live.
unsafe fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device
            .OpenPropertyStore(STGM_READ)
            .context("OpenPropertyStore(STGM_READ) failed")?;

        let mut prop = store
            .GetValue(&PKEY_Device_FriendlyName)
            .context("GetValue(PKEY_Device_FriendlyName) failed")?;

        let variant = &prop.Anonymous.Anonymous;
        let name = if variant.vt == VT_LPWSTR {
            let pwsz = variant.Anonymous.pwszVal;
            if pwsz.is_null() {
                Err(anyhow::anyhow!("friendly name held a null VT_LPWSTR"))
            } else {
                pwsz.to_string()
                    .context("friendly name was not valid UTF-16")
            }
        } else {
            Err(anyhow::anyhow!(
                "friendly name had unexpected variant type {}",
                variant.vt.0
            ))
        };

        // Cleared unconditionally: the error paths above also hold a live
        // allocation.
        let _ = PropVariantClear(&mut prop);
        name
    }
}

/// One session held across the sampling window, with the running peak max.
///
/// Holding the `IAudioSessionControl` (and the meter cast once, up front)
/// keeps every tick cheap and the index stable, so the per-session numbers
/// line up with the endpoint numbers printed beside them.
struct TrackedSession {
    index: i32,
    control: windows::Win32::Media::Audio::IAudioSessionControl,
    meter: Option<IAudioMeterInformation>,
    peak_max: f32,
}

impl TrackedSession {
    fn new(index: i32, control: windows::Win32::Media::Audio::IAudioSessionControl) -> Self {
        let meter = control.cast::<IAudioMeterInformation>().ok();
        Self {
            index,
            control,
            meter,
            peak_max: 0.0,
        }
    }

    fn sample(&mut self) {
        if let Some(m) = &self.meter
            && let Ok(p) = unsafe { m.GetPeakValue() }
        {
            self.peak_max = self.peak_max.max(p);
        }
    }
}

/// Best-effort process name for an audio session's owning PID.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
unsafe fn process_name(pid: u32) -> String {
    if pid == 0 {
        // Sessions rendered by the audio engine itself report PID 0. If A2DP
        // audio lands here, Signal A cannot be session-scoped and must fall
        // back to the endpoint meter.
        return "<audio engine / system>".into();
    }
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return format!("<pid {pid}, not openable>"),
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);

        if result.is_err() || len == 0 {
            return format!("<pid {pid}, name unavailable>");
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit('\\').next().unwrap_or(&full).to_string()
    }
}

fn report_audio_sessions() -> Result<()> {
    println!("\n== layer 3: Signal A - is sound actually coming out ==");

    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

        // eConsole matches what AudioPlaybackConnection renders to: the
        // default render endpoint. The render target is bound when the
        // connection opens and does NOT follow a later default change, which
        // is why a default-device change needs a full reopen.
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default render endpoint")?;

        let name = friendly_name(&device).unwrap_or_else(|e| format!("<{e}>"));
        println!("  default render endpoint: {name}");

        let endpoint_meter: IAudioMeterInformation = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioMeterInformation on the endpoint")?;

        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioSessionManager2")?;

        // Attribution needs per-session peaks measured over the SAME window as
        // the endpoint, not a single read afterwards. Sampling the endpoint
        // first and the sessions second reports every session as silent
        // whenever the audio stops before the second pass - which is exactly
        // what happens with a short burst, and is how the first real run
        // produced six zeroes next to a moving endpoint.
        let sessions = manager
            .GetSessionEnumerator()
            .context("GetSessionEnumerator failed")?;
        let count = sessions.GetCount().context("GetCount failed")?;

        // Snapshot the session list up front so indices stay stable across the
        // window and each one is metered every tick.
        let mut tracked: Vec<TrackedSession> = Vec::new();
        for i in 0..count {
            match sessions.GetSession(i) {
                Ok(control) => tracked.push(TrackedSession::new(i, control)),
                Err(e) => println!("    [{i}] unavailable: {e}"),
            }
        }

        println!("\n  sampling for {SAMPLE_SECS}s (endpoint and every session together):");

        let mut endpoint_max = 0.0f32;
        for _ in 0..SAMPLE_SECS {
            let mut tick_max = 0.0f32;
            // ~10 Hz, the rate the real watchdog will poll at.
            for _ in 0..10 {
                if let Ok(p) = endpoint_meter.GetPeakValue() {
                    tick_max = tick_max.max(p);
                }
                for t in &mut tracked {
                    t.sample();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            endpoint_max = endpoint_max.max(tick_max);
            println!("    endpoint peak (1s max): {tick_max:.6}");
        }
        println!("  endpoint peak over whole window: {endpoint_max:.6}");

        // --- attribution ---------------------------------------------------
        println!("\n  sessions on this endpoint:");

        if count == 0 {
            println!("    (none)");
        }

        for t in &tracked {
            let i = t.index;
            let control = &t.control;

            let control2: IAudioSessionControl2 = match control.cast() {
                Ok(c) => c,
                Err(e) => {
                    println!("    [{i}] no IAudioSessionControl2: {e}");
                    continue;
                }
            };

            let pid = control2.GetProcessId().unwrap_or(0);
            let proc = process_name(pid);
            // IsSystemSoundsSession returns a raw HRESULT: S_OK means yes,
            // S_FALSE means no. BOTH are success codes, so `.is_ok()` is true
            // for every session and would label everything system sounds.
            let is_system = control2.IsSystemSoundsSession() == S_OK;

            let state = match control.GetState() {
                Ok(s) if s == AudioSessionStateActive => "Active",
                Ok(s) if s == AudioSessionStateInactive => "Inactive",
                Ok(s) if s == AudioSessionStateExpired => "Expired",
                Ok(_) => "<unknown>",
                Err(_) => "<error>",
            };

            println!("    [{i}] {proc} (pid {pid})");
            println!(
                "         state: {state}   peak MAX over window: {:.6}{}",
                t.peak_max,
                if is_system { "   [system sounds]" } else { "" }
            );

            // The attribution key. A2DP renders from a protected svchost, so
            // the PID is neither ours nor unique - but the session identifier
            // embeds the endpoint/device string, which we CAN match against
            // the AudioPlaybackConnection device id.
            println!("         id:  {}", session_identifier(&control2));
            println!("         inst:{}", session_instance_identifier(&control2));
        }

        println!(
            "\n  ATTRIBUTION VERDICT: look for a session above that corresponds to\n\
             \x20 the phone's audio while it is streaming. If the only thing moving is\n\
             \x20 the endpoint peak and no session is attributable, Signal A must be\n\
             \x20 endpoint-scoped - which means other apps' audio can mask A2DP\n\
             \x20 silence, and Signal B has to carry more weight in the decision."
        );
    }
    Ok(())
}

// --- `--watch`: a continuous monitor -----------------------------------------

/// One second of observation, rendered as a single line.
struct WatchTick {
    link: String,
    endpoint_peak: f32,
    session: Option<(String, f32)>,
}

pub fn watch(secs: u32) -> Result<()> {
    println!("PurpToof --watch (read-only), {secs}s\n");
    println!(
        "Run this alongside `spike-a2dp --open`, then drive the phone at your\n\
         own pace. One line per second; nothing here advertises a sink or\n\
         opens a stream, so it cannot itself disturb what it is measuring.\n"
    );
    println!("  link      = AudioPlaybackConnection::State() on our own object");
    println!("  ep        = default render endpoint peak, 1s max of ~10 Hz samples");
    println!("  a2dp      = the attributed A2DP session: state and 1s max peak");
    println!("              '-' means no session matched; fall back to ep\n");
    println!("   t   link     ep         a2dp");
    println!("  ---  -------  ---------  --------------------");

    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("could not create IMMDeviceEnumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default render endpoint")?;
        let endpoint_meter: IAudioMeterInformation = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioMeterInformation on the endpoint")?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioSessionManager2")?;

        // Our own connection object, purely to read link state. Constructing
        // one does NOT advertise the PC as a sink - only `Start()` does that,
        // and we never call it.
        let link_conn = link_state_probe();

        for t in 1..=secs {
            let tick = watch_tick(&endpoint_meter, &manager, link_conn.as_ref());
            let a2dp = match &tick.session {
                Some((state, peak)) => format!("{state}/{peak:.6}"),
                None => "-".into(),
            };
            println!(
                "  {t:>3}  {:<7}  {:.6}  {a2dp}",
                tick.link, tick.endpoint_peak
            );
        }
    }

    println!("\nwatch complete");
    Ok(())
}

/// Construct a connection object for link-state reads only, never started.
fn link_state_probe() -> Option<AudioPlaybackConnection> {
    let selector = AudioPlaybackConnection::GetDeviceSelector().ok()?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .ok()?
        .join()
        .ok()?;
    let id = devices.GetAt(0).ok()?.Id().ok()?;
    AudioPlaybackConnection::TryCreateFromId(&id).ok()
}

/// # Safety
///
/// Caller must be on a COM-initialized thread.
unsafe fn watch_tick(
    endpoint_meter: &IAudioMeterInformation,
    manager: &IAudioSessionManager2,
    link_conn: Option<&AudioPlaybackConnection>,
) -> WatchTick {
    unsafe {
        let link = link_conn
            .and_then(|c| c.State().ok())
            .map(state_name)
            .unwrap_or_else(|| "?".into());

        // Re-enumerate every tick so a session appearing or disappearing shows
        // up. That transition is the whole point of watching a disconnect.
        let mut matched: Option<(windows::Win32::Media::Audio::IAudioSessionControl, String)> =
            None;
        if let Ok(sessions) = manager.GetSessionEnumerator()
            && let Ok(count) = sessions.GetCount()
        {
            for i in 0..count {
                let Ok(control) = sessions.GetSession(i) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let id = session_identifier(&control2);
                if looks_like_a2dp_session(&id) {
                    let state = match control.GetState() {
                        Ok(s) if s == AudioSessionStateActive => "Active",
                        Ok(s) if s == AudioSessionStateInactive => "Inactive",
                        Ok(s) if s == AudioSessionStateExpired => "Expired",
                        Ok(_) => "<unknown>",
                        Err(_) => "<error>",
                    };
                    matched = Some((control, state.into()));
                    break;
                }
            }
        }

        let session_meter = matched
            .as_ref()
            .and_then(|(c, _)| c.cast::<IAudioMeterInformation>().ok());

        let mut endpoint_peak = 0.0f32;
        let mut session_peak = 0.0f32;
        for _ in 0..10 {
            if let Ok(p) = endpoint_meter.GetPeakValue() {
                endpoint_peak = endpoint_peak.max(p);
            }
            if let Some(m) = &session_meter
                && let Ok(p) = m.GetPeakValue()
            {
                session_peak = session_peak.max(p);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        WatchTick {
            link,
            endpoint_peak,
            session: matched.map(|(_, state)| (state, session_peak)),
        }
    }
}
