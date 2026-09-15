//! `--debug-sessions`: a read-only dump of what Windows actually believes,
//! at every layer that the phrase "connected" gets applied to.
//!
//! This exists to resolve the two VERIFY items in CLAUDE.md, and to answer a
//! question that comes up constantly in practice: Windows says the phone is
//! connected, the phone disagrees, and neither is lying - they are reporting
//! different things.
//!
//! Four layers, reported separately and never conflated:
//!
//!   1. **Pairing / interface.** Does the A2DP sink interface enumerate at
//!      all? This is what makes the device eligible, nothing more.
//!   2. **Our own sink.** `AudioPlaybackConnection::State()`. Note this
//!      reflects only the connection object *we* hold, not any system-wide
//!      notion of connectedness - if we have not opened it, it reads `Closed`
//!      no matter what Windows Settings displays.
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
//! This code will move to `platform/` behind the `AudioMeter` and
//! `RemotePlayback` traits at milestone 6. It is a flat module for now
//! because the traits it should implement do not exist yet.

use anyhow::{Context, Result};
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
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
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
                "    NOTE: this is OUR connection object, which we have not opened.\n\
                 \x20         `Closed` here is expected and says nothing about what\n\
                 \x20         Windows Settings shows - Settings reports the Bluetooth\n\
                 \x20         link (hands-free, AVRCP, phonebook), not this stream."
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

/// Render a `PWSTR`-returning session getter, freeing the string afterwards.
///
/// These allocate with `CoTaskMemAlloc` and the caller owns the result, so a
/// single named helper is the only place that ownership rule has to be right.
///
/// # Safety
///
/// `f` must return a `PWSTR` that the caller owns, or an error.
unsafe fn pwstr_field(f: impl FnOnce() -> windows::core::Result<PWSTR>) -> String {
    match f() {
        Ok(p) if !p.is_null() => unsafe {
            let s = p.to_string().unwrap_or_else(|e| format!("<{e}>"));
            CoTaskMemFree(Some(p.0 as *const _));
            s
        },
        Ok(_) => "<null>".into(),
        Err(e) => format!("<{e}>"),
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
            println!(
                "         id:  {}",
                pwstr_field(|| control2.GetSessionIdentifier())
            );
            println!(
                "         inst:{}",
                pwstr_field(|| control2.GetSessionInstanceIdentifier())
            );
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

/// Heuristic for "this session is the A2DP render session".
///
/// Attribution cannot use the PID: the owner is a protected `svchost.exe` that
/// `OpenProcess` refuses, and `svchost` would not be unique anyway. The usable
/// key is the session identifier, which embeds the host binary and a grouping
/// GUID. On the rig this was observed as
/// `...\System32\svchost.exe%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}`.
///
/// The GUID's stability across reconnects, reboots and machines is NOT
/// established, so this deliberately matches on the svchost path and treats
/// the GUID as a tiebreaker only. A caller that finds no match must fall back
/// to the endpoint meter rather than concluding the stream is dead.
fn looks_like_a2dp_session(identifier: &str) -> bool {
    let lower = identifier.to_ascii_lowercase();
    lower.contains(r"\system32\svchost.exe")
}

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
                let id = pwstr_field(|| control2.GetSessionIdentifier());
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

#[cfg(test)]
mod tests {
    use super::looks_like_a2dp_session;

    /// The identifier observed on the rig while the phone was streaming.
    const A2DP: &str = concat!(
        r"{0.0.0.00000000}.{a1b9084c-5158-4f1e-83ee-848cb39fdf12}|",
        r"\Device\HarddiskVolume2\Windows\System32\svchost.exe",
        r"%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}"
    );

    #[test]
    fn matches_the_observed_a2dp_session() {
        assert!(looks_like_a2dp_session(A2DP));
    }

    #[test]
    fn does_not_match_ordinary_applications() {
        for other in [
            r"{0.0.0.00000000}.{a1b9084c}|\Device\HarddiskVolume2\Program Files (x86)\Steam\steam.exe%b{0}",
            r"{0.0.0.00000000}.{a1b9084c}|\Device\HarddiskVolume5\SteamLibrary\steamapps\common\Call of Duty 4\iw3sp.exe%b{0}",
            r"{0.0.0.00000000}.{a1b9084c}|#%b{A9EF3FD9-4240-455E-A4D5-F2B3301887B2}",
        ] {
            assert!(!looks_like_a2dp_session(other), "wrongly matched: {other}");
        }
    }

    #[test]
    fn is_case_insensitive_on_the_path() {
        assert!(looks_like_a2dp_session(
            r"x|\Device\HarddiskVolume2\WINDOWS\SYSTEM32\SVCHOST.EXE%b{C55CBD10}"
        ));
    }
}
