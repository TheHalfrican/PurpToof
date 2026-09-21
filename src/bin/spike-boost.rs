//! Volume-boost spike. Answers the question that gates whether PurpToof can
//! make audio LOUDER than the phone sends it:
//!
//!   Every Windows volume API stops at unity. `ISimpleAudioVolume` is clamped
//!   to [0.0, 1.0] and `IAudioEndpointVolume`'s maximum is 0 dB, by
//!   definition. Going past 100% means multiplying PCM samples by more than
//!   1.0 - and CLAUDE.md's hard constraint says `AudioPlaybackConnection`
//!   never hands us the PCM.
//!
//!   Process loopback capture is the only user-mode way to get it: no driver,
//!   no APO, no MSIX. Its floor is Windows 10 build 19041, which is already
//!   this project's floor.
//!
//! # The four questions
//!
//! 1. **Where is the attenuation?** If the phone is sending at -12 dBFS, the
//!    fix is the phone's volume slider and no amount of PC-side gain recovers
//!    the headroom cleanly. Read the A2DP session's own volume, the endpoint
//!    master, and the arriving peak, and the answer is obvious in one glance.
//!
//! 2. **Does process-loopback activation survive the protected `svchost`?**
//!    A2DP render audio lands on a protected service host. We know
//!    `GetProcessId` works there and `OpenProcess` does not -
//!    `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS.TargetProcessId` is a raw `u32`
//!    and needs no handle, so this *should* work. Should is not a measurement.
//!
//! 3. **Does muting the A2DP session kill its peak meter?** This is the one
//!    that can sink the whole feature. The boost pipeline must mute the
//!    original session or you hear it twice, and that session's meter is
//!    Signal A - the only health signal this app has left, now that Signal B
//!    is known absent on this hardware. If mute zeroes the meter and we have
//!    not replaced it first, PurpToof stops being self-healing.
//!
//! 4. **Do packets keep arriving during digital silence?** `GetPeakValue`
//!    reads 0.0 for "paused" and for "audio path is dead" alike, which is
//!    exactly why the watchdog is inert. A capture client may be able to tell
//!    them apart: frames arriving with `AUDCLNT_BUFFERFLAGS_SILENT` means the
//!    stream is alive and quiet; no frames arriving at all means it is dead.
//!    If that holds, this spike found the discriminator Signal B was supposed
//!    to provide, and the volume feature is the lesser half of the result.
//!
//! # Staging, and why it exists
//!
//! Same convention as `spike-a2dp`: the default is read-only, and anything
//! that can disturb a machine someone is using is opt-in behind a flag.
//!
//! ```text
//! cargo run --bin spike-boost                  # read-only inventory
//! cargo run --bin spike-boost -- --loopback=10 # capture only; playback untouched
//! cargo run --bin spike-boost -- --mute-probe=6 # INTRUSIVE: mutes the phone audio
//! ```
//!
//! `--loopback` is non-intrusive: capturing a session does not remove its
//! audio from the endpoint. `--mute-probe` genuinely silences the phone for
//! the duration and then restores the previous state, including on error, via
//! a `Drop` guard. It is not Ctrl-C safe, so it keeps the window short.

use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use purptoof::platform::a2dp_session::{looks_like_a2dp_session, session_identifier};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, AudioSessionStateActive, AudioSessionStateExpired,
    AudioSessionStateInactive, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

/// `AUDCLNT_BUFFERFLAGS_SILENT`. Spelled out rather than imported because
/// windows-rs surfaces the buffer flags as a bare `u32` out-parameter, so an
/// imported newtype would have to be unwrapped at the comparison anyway.
const BUFFERFLAGS_SILENT: u32 = 0x2;

/// `WAVE_FORMAT_IEEE_FLOAT`. Process loopback does **not** support
/// `GetMixFormat` - the caller states the format it wants and the engine
/// converts. 32-bit float is the engine's native mix format, so asking for it
/// avoids a conversion and makes peak computation a straight `f32` read.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let loopback = flag_secs(&args, "--loopback");
    let mute_probe = flag_secs(&args, "--mute-probe");
    let exclude_self = args.iter().any(|a| a == "--exclude-self");
    let endpoint_loopback = flag_secs(&args, "--endpoint-loopback");

    for a in &args {
        if !a.starts_with("--loopback")
            && !a.starts_with("--mute-probe")
            && !a.starts_with("--endpoint-loopback")
            && a != "--exclude-self"
        {
            bail!(
                "unknown argument {a}\n\nusage: spike-boost [--loopback[=SECS]] [--mute-probe[=SECS]]"
            );
        }
    }

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx failed")?;
    }

    let inventory = unsafe { inventory() }?;

    match (loopback, mute_probe, endpoint_loopback) {
        (None, None, None) => {
            println!(
                "\nRead-only. Add --loopback=10 to try process loopback capture, \
                 or --mute-probe=6 to answer whether muting kills Signal A.\n\
                 --mute-probe SILENCES THE PHONE for its duration."
            );
        }
        _ => {
            if let Some(secs) = loopback {
                unsafe { loopback_probe(&inventory, secs, exclude_self) }?;
            }
            if let Some(secs) = endpoint_loopback {
                unsafe { endpoint_loopback_probe(&inventory, secs) }?;
            }
            if let Some(secs) = mute_probe {
                unsafe { mute_probe_run(&inventory, secs) }?;
            }
        }
    }

    Ok(())
}

/// Parse `--name` (defaulting to 8 seconds) or `--name=SECS`.
fn flag_secs(args: &[String], name: &str) -> Option<u64> {
    args.iter().find_map(|a| {
        if a == name {
            Some(8)
        } else {
            a.strip_prefix(name)
                .and_then(|r| r.strip_prefix('='))
                .and_then(|n| n.parse().ok())
        }
    })
}

/// One candidate A2DP render session, with every interface the spike needs.
struct A2dpSession {
    pid: u32,
    state: &'static str,
    identifier: String,
    volume: Option<ISimpleAudioVolume>,
    meter: Option<IAudioMeterInformation>,
}

struct Inventory {
    sessions: Vec<A2dpSession>,
}

/// Question 1: where is the attenuation? Read-only.
///
/// # Safety
///
/// COM must be initialised on this thread.
unsafe fn inventory() -> Result<Inventory> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("could not create IMMDeviceEnumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default render endpoint")?;

        println!("=== layer 1: the endpoint ===");
        match device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            Ok(vol) => {
                let scalar = vol.GetMasterVolumeLevelScalar().unwrap_or(-1.0);
                let (mut min, mut max, mut inc) = (0.0f32, 0.0f32, 0.0f32);
                let range = vol.GetVolumeRange(&mut min, &mut max, &mut inc).is_ok();
                println!("  master volume      : {:.1}%", scalar * 100.0);
                if range {
                    // Expected to print max = 0.0 dB. That IS the proof that no
                    // Windows volume API can boost: unity is the ceiling.
                    println!("  volume range       : {min:.1} dB .. {max:.1} dB (step {inc:.2})");
                }
                println!(
                    "  muted              : {}",
                    vol.GetMute().unwrap_or_default().as_bool()
                );
            }
            Err(e) => println!("  <IAudioEndpointVolume unavailable: {e}>"),
        }
        if let Ok(meter) = device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) {
            println!(
                "  endpoint peak      : {:.4}",
                meter.GetPeakValue().unwrap_or(0.0)
            );
        }

        println!("\n=== layer 2: the A2DP render session(s) ===");
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioSessionManager2")?;
        let sessions = manager
            .GetSessionEnumerator()
            .context("could not enumerate audio sessions")?;
        let count = sessions.GetCount().unwrap_or(0);

        let mut found = Vec::new();
        for i in 0..count {
            let Ok(control) = sessions.GetSession(i) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let identifier = session_identifier(&control2);
            if !looks_like_a2dp_session(&identifier) {
                continue;
            }

            let state = match control.GetState() {
                Ok(s) if s == AudioSessionStateActive => "Active",
                Ok(s) if s == AudioSessionStateInactive => "Inactive",
                Ok(s) if s == AudioSessionStateExpired => "Expired",
                _ => "<unknown>",
            };

            found.push(A2dpSession {
                pid: control2.GetProcessId().unwrap_or(0),
                state,
                identifier,
                volume: control.cast::<ISimpleAudioVolume>().ok(),
                meter: control.cast::<IAudioMeterInformation>().ok(),
            });
        }

        if found.is_empty() {
            println!(
                "  none. {count} sessions on the endpoint, no svchost match.\n\
                 \n  Connect the phone and start audio, then re-run. Every question\n\
                 below needs a live A2DP session to mean anything."
            );
        }

        for (i, s) in found.iter().enumerate() {
            println!("\n  [{i}] pid {} ({})", s.pid, s.state);
            println!("      identifier     : {}", s.identifier);
            match &s.volume {
                // If this reads below 1.0, there is free gain available with
                // no DSP at all - and the "boost" is really an unmute.
                Some(v) => println!(
                    "      session volume : {:.1}%  muted={}",
                    v.GetMasterVolume().unwrap_or(-1.0) * 100.0,
                    v.GetMute().unwrap_or_default().as_bool()
                ),
                None => println!("      session volume : <ISimpleAudioVolume unavailable>"),
            }
            match &s.meter {
                Some(m) => println!(
                    "      peak           : {:.4}",
                    m.GetPeakValue().unwrap_or(0.0)
                ),
                None => println!("      peak           : <IAudioMeterInformation unavailable>"),
            }
        }

        Ok(Inventory { sessions: found })
    }
}

/// Completion handler for `ActivateAudioInterfaceAsync`.
///
/// The activation is genuinely asynchronous even though everything around it
/// here is blocking, so the handler does nothing but wake the caller. The
/// result is read from the operation object afterwards, on our thread.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    tx: Mutex<Option<Sender<()>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Ok(mut guard) = self.tx.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// Native `PROPVARIANT` holding a `VT_BLOB`, laid out by hand.
///
/// windows-rs models `PROPVARIANT` as an opaque managed type with no way to
/// construct a blob variant, and `ActivateAudioInterfaceAsync` takes a raw
/// pointer - so the honest move is to build the 24-byte x64 layout ourselves
/// and cast. Two `u16` pairs, then `cbSize` with four bytes of padding before
/// the pointer, which is where a hand-rolled layout normally goes wrong.
#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    reserved1: u16,
    reserved2: u16,
    reserved3: u16,
    cb_size: u32,
    _pad: u32,
    blob_data: *mut u8,
}

/// `VT_BLOB`.
const VT_BLOB: u16 = 65;

/// Questions 2 and 4: does process loopback work against the protected
/// `svchost`, and do packets keep arriving while the stream is silent?
///
/// Capturing does not remove audio from the endpoint, so this is safe to run
/// on a machine in use.
///
/// # Safety
///
/// COM must be initialised on this thread.
unsafe fn loopback_probe(inv: &Inventory, secs: u64, exclude_self: bool) -> Result<()> {
    let Some(target) = inv.sessions.iter().find(|s| s.state == "Active") else {
        let hint = if inv.sessions.is_empty() {
            "no A2DP session at all"
        } else {
            "every A2DP session is Inactive or Expired"
        };
        bail!("nothing to capture: {hint}. Start audio on the phone and re-run.");
    };

    println!(
        "\n=== question 2: process loopback against pid {} ===",
        target.pid
    );

    unsafe {
        // INCLUDE is the shipping design: we want this one service's audio and
        // nothing else, because EXCLUDE would sweep up the browser and every
        // game, which is a system-wide booster and somebody else's app.
        //
        // EXCLUDE targeting OURSELVES is the control. It captures everything on
        // the endpoint, so if it hears audio while INCLUDE hears silence, the
        // samples are real and only the per-process attribution is failing -
        // which is a completely different problem from "loopback is blocked".
        let (pid, mode) = if exclude_self {
            (
                GetCurrentProcessId(),
                PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            )
        } else {
            (
                target.pid,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            )
        };
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        let pv = PropVariantBlob {
            vt: VT_BLOB,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            cb_size: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            _pad: 0,
            blob_data: (&raw mut params).cast::<u8>(),
        };

        let (tx, rx) = channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Completion {
            tx: Mutex::new(Some(tx)),
        }
        .into();

        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const pv).cast::<PROPVARIANT>()),
            &handler,
        )
        .context("ActivateAudioInterfaceAsync refused the call outright")?;

        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("activation never completed - handler was not called"))?;

        let mut hr = HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unknown)
            .context("GetActivateResult failed")?;
        if let Err(e) = hr.ok() {
            // This is the answer to question 2 if it fires: the protected
            // service host refused us, and the whole boost design dies here.
            bail!("activation returned {hr:?}: {e}\n\nQUESTION 2 ANSWERED: NO.");
        }
        let client: IAudioClient = unknown
            .ok_or_else(|| anyhow!("activation succeeded but returned no interface"))?
            .cast()
            .context("activated interface was not an IAudioClient")?;

        println!("  activation         : OK  <- QUESTION 2 ANSWERED: YES");

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                2_000_000, // 200 ms, in 100 ns units
                0,
                &format,
                None,
            )
            .context("IAudioClient::Initialize failed on the loopback client")?;

        let event: HANDLE =
            CreateEventW(None, false, false, None).context("CreateEventW failed")?;
        client
            .SetEventHandle(event)
            .context("SetEventHandle failed")?;
        let capture: IAudioCaptureClient = client.GetService().context("no IAudioCaptureClient")?;
        client.Start().context("IAudioClient::Start failed")?;

        println!("\n=== question 4: packet arrival over {secs}s ===");
        println!("  Pause and unpause the phone during this window.\n");

        let mut packets = 0u64;
        let mut silent_packets = 0u64;
        let mut frames_total = 0u64;
        let mut peak_overall = 0.0f32;
        let mut last_print = Instant::now();
        let mut window_peak = 0.0f32;
        let mut window_packets = 0u64;
        let mut window_silent = 0u64;

        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            WaitForSingleObject(event, 250);
            loop {
                let mut data: *mut u8 = null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if frames == 0 {
                    let _ = capture.ReleaseBuffer(0);
                    break;
                }

                packets += 1;
                window_packets += 1;
                frames_total += u64::from(frames);
                if flags & BUFFERFLAGS_SILENT != 0 {
                    silent_packets += 1;
                    window_silent += 1;
                } else if !data.is_null() {
                    let samples =
                        std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * 2);
                    for s in samples {
                        let a = s.abs();
                        if a > window_peak {
                            window_peak = a;
                        }
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
            }

            if last_print.elapsed() >= Duration::from_secs(1) {
                // The discriminator, printed once a second. "packets>0 peak=0"
                // is a live-but-quiet stream; "packets=0" is a dead one. If
                // those two states are distinguishable here, the watchdog can
                // be made to work without Signal B.
                // The session meter, read in the same second as the captured
                // peak. Comparing them across separate runs proves nothing -
                // the music simply may have been quiet. Side by side, a
                // disagreement is a real finding.
                let session_peak = target
                    .meter
                    .as_ref()
                    .and_then(|m| m.GetPeakValue().ok())
                    .unwrap_or(-1.0);
                println!(
                    "  packets={window_packets:<4} silent={window_silent:<4}                      captured={window_peak:.4}  session_meter={session_peak:.4}"
                );
                peak_overall = peak_overall.max(window_peak);
                window_peak = 0.0;
                window_packets = 0;
                window_silent = 0;
                last_print = Instant::now();
            }
        }

        let _ = client.Stop();
        println!(
            "\n  totals: {packets} packets, {silent_packets} flagged silent, \
             {frames_total} frames, peak {peak_overall:.4}"
        );
        if packets == 0 {
            println!(
                "  NO PACKETS AT ALL. Either the target renders nothing, or process\n\
                 loopback does not observe this service. Question 4 is unanswered."
            );
        }
    }

    Ok(())
}

/// The OTHER loopback: classic endpoint loopback, which taps the final mix of
/// the render endpoint rather than filtering per process.
///
/// Worth measuring even though it cannot support a *targeted* boost - it
/// captures every application at once, and muting A2DP to avoid hearing it
/// twice would also remove it from the very mix being captured. Its value is
/// diagnostic: "invisible to process loopback but present in the endpoint
/// mix" and "invisible to both" are different facts about the stream, and the
/// second one says the samples are deliberately withheld rather than merely
/// unattributable.
///
/// # Safety
///
/// COM must be initialised on this thread.
unsafe fn endpoint_loopback_probe(inv: &Inventory, secs: u64) -> Result<()> {
    println!(
        "
=== control: CLASSIC endpoint loopback (whole mix) ==="
    );
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate IAudioClient on the endpoint")?;
        let fmt_ptr = client.GetMixFormat().context("GetMixFormat failed")?;
        // WAVEFORMATEX is `#[repr(packed)]`, so every field has to be copied
        // to a local before it can be formatted - a format argument takes a
        // reference, and a reference to a misaligned field is UB.
        let fmt = *fmt_ptr;
        let (ch, hz, bits, tag) = (
            fmt.nChannels,
            fmt.nSamplesPerSec,
            fmt.wBitsPerSample,
            fmt.wFormatTag,
        );
        println!("  mix format         : {ch} ch, {hz} Hz, {bits} bit (tag {tag})");
        let channels = ch as usize;
        let is_f32 = bits == 32;

        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            2_000_000,
            0,
            fmt_ptr,
            None,
        );
        CoTaskMemFree(Some(fmt_ptr.cast()));
        init.context("Initialize failed on the endpoint loopback client")?;

        let capture: IAudioCaptureClient = client.GetService().context("no IAudioCaptureClient")?;
        client.Start().context("Start failed")?;

        let mut window_peak = 0.0f32;
        let mut window_packets = 0u64;
        let mut overall = 0.0f32;
        let mut last_print = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            loop {
                let mut data: *mut u8 = null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if frames == 0 {
                    let _ = capture.ReleaseBuffer(0);
                    break;
                }
                window_packets += 1;
                if flags & BUFFERFLAGS_SILENT == 0 && is_f32 && !data.is_null() {
                    let samples =
                        std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * channels);
                    for v in samples {
                        window_peak = window_peak.max(v.abs());
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
            }
            if last_print.elapsed() >= Duration::from_secs(1) {
                let session_peak = inv
                    .sessions
                    .iter()
                    .filter_map(|s| s.meter.as_ref())
                    .filter_map(|m| m.GetPeakValue().ok())
                    .fold(0.0f32, f32::max);
                println!(
                    "  packets={window_packets:<4} captured={window_peak:.4}                       session_meter={session_peak:.4}"
                );
                overall = overall.max(window_peak);
                window_peak = 0.0;
                window_packets = 0;
                last_print = Instant::now();
            }
        }
        let _ = client.Stop();
        println!(
            "
  endpoint-loopback peak over {secs}s: {overall:.4}"
        );
    }
    Ok(())
}

/// Restores a session's mute state on the way out, including on panic or on
/// an early return. The probe deliberately silences the phone; leaving it
/// silenced because an error path forgot to undo it is not acceptable.
struct MuteGuard<'a> {
    volume: &'a ISimpleAudioVolume,
    previous: bool,
}

impl Drop for MuteGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.volume.SetMute(self.previous, std::ptr::null());
        }
        println!("  mute restored to {}", self.previous);
    }
}

/// Question 3: does muting the A2DP session zero its peak meter?
///
/// If it does, the boost pipeline cannot mute the original without destroying
/// Signal A, and the capture-derived meter has to land *first*.
///
/// # Safety
///
/// COM must be initialised on this thread.
unsafe fn mute_probe_run(inv: &Inventory, secs: u64) -> Result<()> {
    let Some(target) = inv
        .sessions
        .iter()
        .find(|s| s.state == "Active" && s.volume.is_some() && s.meter.is_some())
    else {
        bail!(
            "no Active A2DP session with both ISimpleAudioVolume and a meter. \
             Start audio on the phone and re-run."
        );
    };
    let volume = target.volume.as_ref().expect("filtered on is_some");
    let meter = target.meter.as_ref().expect("filtered on is_some");

    let half = secs.max(2) / 2;
    println!("\n=== question 3: does mute kill the session meter? ===");
    println!("  THIS SILENCES THE PHONE for ~{half}s. Keep audio playing.\n");

    unsafe {
        println!("  -- unmuted, {half}s --");
        let baseline = sample_peak(meter, half);

        let previous = volume.GetMute().unwrap_or_default().as_bool();
        volume
            .SetMute(true, std::ptr::null())
            .context("SetMute(true) was refused on the protected session")?;
        let _guard = MuteGuard { volume, previous };

        println!("\n  -- muted, {half}s --");
        let muted = sample_peak(meter, half);

        println!("\n  baseline peak: {baseline:.4}");
        println!("  muted peak   : {muted:.4}");
        if baseline < 0.0005 {
            println!(
                "\n  INCONCLUSIVE: the baseline was already silent, so this run \
                 proves nothing.\n  Make sure audio is actually playing and re-run."
            );
        } else if muted < 0.0005 {
            println!(
                "\n  QUESTION 3 ANSWERED: mute ZEROES the meter.\n  \
                 The boost pipeline must derive Signal A from captured PCM \
                 BEFORE it is allowed to mute anything."
            );
        } else {
            println!(
                "\n  QUESTION 3 ANSWERED: the meter survives mute.\n  \
                 Signal A and the boost pipeline can coexist without reordering."
            );
        }
    }

    Ok(())
}

/// Poll a meter for `secs` at ~10 Hz, printing once a second, and return the
/// peak seen.
///
/// # Safety
///
/// `meter` must be a live interface.
unsafe fn sample_peak(meter: &IAudioMeterInformation, secs: u64) -> f32 {
    let mut overall = 0.0f32;
    let mut window = 0.0f32;
    let mut last_print = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        let p = unsafe { meter.GetPeakValue().unwrap_or(0.0) };
        window = window.max(p);
        if last_print.elapsed() >= Duration::from_secs(1) {
            println!("    peak {window:.4}");
            overall = overall.max(window);
            window = 0.0;
            last_print = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    overall.max(window)
}
