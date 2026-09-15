//! Packaging spike. Answers the one question that gates the whole project:
//!
//!   Does `AudioPlaybackConnection` work from an ORDINARY UNPACKAGED Win32
//!   exe, or does the `bluetooth` DeviceCapability check force us into a
//!   sparse MSIX package?
//!
//! # Staging, and why it exists
//!
//! Stage 1 is read-only. It enumerates and constructs the connection object
//! but never advertises the PC as a sink, so a paired phone cannot latch on
//! and redirect its audio to the default output of a machine someone is
//! actively using.
//!
//! Stage 2 (`--open`) calls `Start`, which DOES advertise this PC as an
//! available A2DP sink, and `Open`, which waits for the remote to connect and
//! then opens the audio stream. It is therefore opt-in, never the default.
//!
//! # Where the capability gate can fire
//!
//! Two places, and stage 1 only clears the first:
//!
//!   1. `TryCreateFromId` - an access-denied HRESULT or a null return.
//!   2. `Open` - `AudioPlaybackConnectionOpenResultStatus::DeniedBySystem`.
//!
//! That second status exists in the metadata specifically to express "the
//! system refused you", so a clean stage 1 is suggestive but not a verdict.

use anyhow::{Context, Result, bail};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Media::Audio::{
    AudioPlaybackConnection, AudioPlaybackConnectionOpenResultStatus, AudioPlaybackConnectionState,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

/// Named rendering of the state newtype. windows-rs generates these as
/// associated consts on a tuple struct, so `Debug` only ever shows the
/// integer, and using them as match patterns is fragile.
fn state_name(s: AudioPlaybackConnectionState) -> String {
    if s == AudioPlaybackConnectionState::Closed {
        "Closed".into()
    } else if s == AudioPlaybackConnectionState::Opened {
        "Opened".into()
    } else {
        format!("<unknown {}>", s.0)
    }
}

fn open_status_name(s: AudioPlaybackConnectionOpenResultStatus) -> String {
    if s == AudioPlaybackConnectionOpenResultStatus::Success {
        "Success".into()
    } else if s == AudioPlaybackConnectionOpenResultStatus::RequestTimedOut {
        "RequestTimedOut".into()
    } else if s == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem {
        "DeniedBySystem".into()
    } else if s == AudioPlaybackConnectionOpenResultStatus::UnknownFailure {
        "UnknownFailure".into()
    } else {
        format!("<unknown {}>", s.0)
    }
}

/// Seconds `--open` holds the stream up when `--hold` is not given.
const DEFAULT_HOLD_SECS: u32 = 20;

/// Parse `--hold=N` or `--hold N`. Returns `None` when the flag is absent or
/// its value does not parse, and the caller falls back to the default - a
/// malformed hold is not worth aborting a hardware run over.
fn parse_hold(args: impl Iterator<Item = String>) -> Option<u32> {
    let mut rest = args.skip_while(|a| !a.starts_with("--hold"));
    let flag = rest.next()?;
    match flag.strip_prefix("--hold=") {
        Some(v) => v.parse().ok(),
        None => rest.next()?.parse().ok(),
    }
}

/// Never reopen faster than this. `Open()` can return immediately when no
/// remote is reachable, and a bare loop around it would spin the radio.
const REARM_FLOOR: std::time::Duration = std::time::Duration::from_millis(250);

/// How long to wait for `StateChanged -> Opened` after `Open()` says `Success`.
/// `Success` is not the transition - see docs/verify.md - so a success with no
/// transition inside this window is a failed arm, not a live link.
const OPEN_TRANSITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hold the sink permanently armed: keep an `Open()` in flight, and the
/// instant the link closes, open again.
///
/// Every line is timestamped from the start of the run so the log can be read
/// against what the operator was doing on the phone. The question it answers is
/// narrow: after a link closes, does an `Open()` that nobody prompted complete
/// on its own when audio starts on the phone?
fn rearm_loop(connection: &AudioPlaybackConnection, total_secs: u32) -> Result<()> {
    let run_start = std::time::Instant::now();
    let deadline = run_start + std::time::Duration::from_secs(total_secs as u64);
    let stamp = |t: &std::time::Instant| format!("t+{:>5.1}s", t.elapsed().as_secs_f32());

    println!("\n== always-armed sink, {total_secs}s ==");
    println!(
        "Start() is held for the whole run, so the PC never stops advertising.\n\
         Open() is reissued the moment the link closes.\n"
    );

    let mut arm = 0u32;
    while std::time::Instant::now() < deadline {
        arm += 1;
        let attempt_at = std::time::Instant::now();
        println!("[{}] arm #{arm}: Open() ...", stamp(&run_start));

        let result = connection.Open().context("Open dispatch failed")?;
        let status = result.Status()?;
        let waited = attempt_at.elapsed();

        if status != AudioPlaybackConnectionOpenResultStatus::Success {
            // The extended error is the only thing that distinguishes one
            // UnknownFailure from another, and "no phone reachable" arrives as
            // UnknownFailure rather than RequestTimedOut - so this HRESULT is
            // what a non-escalating mapping has to key on.
            println!(
                "[{}] arm #{arm}: {} after {:.1}s (extended {:?}) - rearming",
                stamp(&run_start),
                open_status_name(status),
                waited.as_secs_f32(),
                result.ExtendedError()
            );
            std::thread::sleep(REARM_FLOOR);
            continue;
        }

        println!(
            "[{}] arm #{arm}: Success after {:.1}s - waiting for Opened",
            stamp(&run_start),
            waited.as_secs_f32()
        );

        // `Success` is a dispatch result, not a transition. Wait for the real
        // thing before calling the link live.
        let transition_start = std::time::Instant::now();
        let mut went_open = false;
        while transition_start.elapsed() < OPEN_TRANSITION_TIMEOUT {
            if connection.State()? == AudioPlaybackConnectionState::Opened {
                went_open = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        if !went_open {
            println!(
                "[{}] arm #{arm}: Success but NO Opened transition in {:?} - failed arm",
                stamp(&run_start),
                OPEN_TRANSITION_TIMEOUT
            );
            std::thread::sleep(REARM_FLOOR);
            continue;
        }

        println!(
            "[{}] arm #{arm}: LINK UP after {:.1}s",
            stamp(&run_start),
            transition_start.elapsed().as_secs_f32()
        );

        // Sit on the live link until it drops or the run ends.
        let up_since = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            if connection.State()? != AudioPlaybackConnectionState::Opened {
                println!(
                    "[{}] arm #{arm}: LINK DOWN after {:.1}s up - rearming immediately",
                    stamp(&run_start),
                    up_since.elapsed().as_secs_f32()
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    println!("\n[{}] run complete, {arm} arm(s)", stamp(&run_start));
    Ok(())
}

fn main() -> Result<()> {
    // WinRT activation needs an initialized apartment. MTA is correct for a
    // console process with no message pump.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx failed")?;

    // `--start-only` is the interesting middle rung. `Start` is the call that
    // actually exercises the radio - it advertises this PC as an available
    // A2DP sink - so it is where a `bluetooth` DeviceCapability check has to
    // bite if it is enforced at all. But it does NOT open a stream, so no
    // audio can reach the default render endpoint. That makes it safe to run
    // on a machine in use, while still being the strongest evidence available
    // short of a full open.
    let start_only = std::env::args().any(|a| a == "--start-only");
    let open = std::env::args().any(|a| a == "--open");

    // How long `--open` holds the stream up after a successful `Open()`. The
    // default is enough to hear whether audio is flowing; a longer hold is for
    // running `--debug-sessions` against a live stream from another terminal,
    // which needs room for enumeration plus its own sampling window.
    let hold_secs = parse_hold(std::env::args()).unwrap_or(DEFAULT_HOLD_SECS);

    // `--rearm` is the always-armed sink: Start once, then keep an Open in
    // flight forever, reopening the instant the link closes. It exists to
    // answer whether iOS re-routes to the PC by itself once a link has
    // dropped - which decides whether "audio always resumes" is achievable
    // from this side alone, or needs a tap in Control Center. It is also a
    // miniature of the milestone-6 re-arm path.
    let rearm = std::env::args().any(|a| a == "--rearm");

    println!("== stage 1: enumeration + construction (read-only) ==");

    // Reaching this call at all proves the WinRT class activates unpackaged.
    let selector = AudioPlaybackConnection::GetDeviceSelector()
        .context("GetDeviceSelector failed - class did not activate unpackaged")?;
    println!("selector: {selector}");

    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .context("FindAllAsyncAqsFilter dispatch failed")?
        .join()
        .context("FindAllAsyncAqsFilter did not complete")?;

    let count = devices.Size().context("device collection has no size")?;
    println!("found {count} candidate device(s)");
    if count == 0 {
        bail!(
            "no A2DP source devices found. Pair a phone with this PC first; \
             the selector only matches already-paired devices."
        );
    }

    let mut first = None;
    for device in &devices {
        let id = device.Id().context("device has no Id")?;
        let name = device.Name().context("device has no Name")?;
        println!("  - {name}\n      id: {id}");
        if first.is_none() {
            first = Some((id, name));
        }
    }

    // Gate #1.
    let (id, name) = first.expect("count > 0 was checked above");
    println!("\nTryCreateFromId against: {name}");

    let connection = match AudioPlaybackConnection::TryCreateFromId(&id) {
        Ok(c) => {
            println!("  -> OK: connection object constructed with NO package identity");
            c
        }
        Err(e) => {
            println!("  -> FAILED: {e}");
            println!("     hresult: {:?}", e.code());
            println!(
                "\nVERDICT: unpackaged construction is refused. Sparse MSIX is\n\
                 required; confirm an identity-only package satisfies the\n\
                 capability before writing any more code."
            );
            return Ok(());
        }
    };

    println!("  device id round-trip: {}", connection.DeviceId()?);
    println!("  initial state: {}", state_name(connection.State()?));

    if !open && !start_only && !rearm {
        // Drop without ever advertising. Nothing on the air learned that this
        // PC is willing to be a speaker.
        drop(connection);
        println!(
            "\nstage 1 PASSED, but this is NOT the final verdict: the gate can\n\
             still fire at Start() or at Open() as DeniedBySystem.\n\
             Next: --start-only (no audio moves), then --open (audio moves)."
        );
        return Ok(());
    }

    // --- Stage 2 ------------------------------------------------------------
    if start_only {
        println!("\n== stage 1b: Start only (advertises the radio, moves no audio) ==");
    } else {
        println!("\n== stage 2: Start + Open (WILL affect system audio) ==");
    }

    // Log link-state transitions. This is a LINK signal, not a flow signal.
    // The entire premise of this project is that it can report Opened while no
    // audio is moving, so it is here to be logged, never to decide health.
    let token = connection.StateChanged(&windows::Foundation::TypedEventHandler::<
        AudioPlaybackConnection,
        windows::core::IInspectable,
    >::new(|sender, _| {
        if let Some(s) = sender.as_ref() {
            println!("  [event] StateChanged -> {}", state_name(s.State()?));
        }
        Ok(())
    }))?;

    connection
        .Start()
        .context("Start failed - the capability gate refusing to advertise")?;
    println!("Start OK - PC is now advertising as an A2DP sink");

    if start_only {
        println!(
            "\nThe capability gate did NOT fire on the call that uses the radio.\n\
             Watching link state for 8s without opening a stream."
        );
        for i in 1..=8 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if i % 4 == 0 {
                println!("  t+{i}s state: {}", state_name(connection.State()?));
            }
        }
        connection.Close().ok();
        connection.RemoveStateChanged(token).ok();
        println!(
            "\nclosed, nothing advertised any more.\n\
             Strong evidence that unpackaged Win32 is sufficient. Confirm with\n\
             --open (which does route phone audio to the default output)."
        );
        return Ok(());
    }

    if rearm {
        let r = rearm_loop(&connection, hold_secs);
        connection.Close().ok();
        connection.RemoveStateChanged(token).ok();
        println!("closed");
        return r;
    }

    println!(
        "\n>>> On the iPhone now: Settings > Bluetooth > this PC, or pick the PC\n\
         >>> as the audio output. Open() blocks until the remote connects or\n\
         >>> the request times out."
    );

    // Gate #2. Blocks until the remote connects or the request times out.
    let result = connection.Open().context("Open dispatch failed")?;
    let status = result.Status()?;
    println!("\nOpen status: {}", open_status_name(status));
    println!("  extended error: {:?}", result.ExtendedError()?);
    println!("  state: {}", state_name(connection.State()?));

    if status == AudioPlaybackConnectionOpenResultStatus::DeniedBySystem {
        println!(
            "\nVERDICT: DeniedBySystem. The bluetooth DeviceCapability IS enforced\n\
             at Open() for an unpackaged process. Sparse MSIX is required."
        );
    } else if status == AudioPlaybackConnectionOpenResultStatus::Success {
        println!(
            "\nVERDICT: unpackaged Win32 is sufficient. MSIX is OFF the table -\n\
             no package identity, no UWP lifecycle, no suspension risk."
        );
        println!("\nholding {hold_secs}s - play audio on the phone and listen.");
        for i in 1..=hold_secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if i % 5 == 0 {
                println!("  t+{i}s state: {}", state_name(connection.State()?));
            }
        }
    } else {
        println!(
            "\nINCONCLUSIVE: {} is not a capability refusal. Most likely the phone\n\
             never initiated the connection within the timeout. Re-run and connect\n\
             it promptly.",
            open_status_name(status)
        );
    }

    connection.Close().ok();
    connection.RemoveStateChanged(token).ok();
    println!("closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(args: &[&str]) -> Option<u32> {
        parse_hold(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn absent_flag_falls_through_to_the_default() {
        assert_eq!(hold(&["spike-a2dp", "--open"]), None);
    }

    #[test]
    fn both_spellings_parse() {
        assert_eq!(hold(&["spike-a2dp", "--open", "--hold=90"]), Some(90));
        assert_eq!(hold(&["spike-a2dp", "--open", "--hold", "90"]), Some(90));
    }

    #[test]
    fn malformed_values_fall_through_rather_than_abort() {
        assert_eq!(hold(&["spike-a2dp", "--hold=abc"]), None);
        assert_eq!(hold(&["spike-a2dp", "--hold"]), None);
        assert_eq!(hold(&["spike-a2dp", "--hold", "--open"]), None);
    }
}
