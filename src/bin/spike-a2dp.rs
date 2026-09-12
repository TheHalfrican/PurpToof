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

    if !open && !start_only {
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
        println!("\nholding 20s - play audio on the phone and listen.");
        for i in 1..=20 {
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
