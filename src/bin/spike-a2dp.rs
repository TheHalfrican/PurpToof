//! Packaging spike. Answers the one question that gates the whole project:
//!
//!   Does `AudioPlaybackConnection` work from an ORDINARY UNPACKAGED Win32
//!   exe, or does the `bluetooth` DeviceCapability check force us into a
//!   sparse MSIX package?
//!
//! Run it unpackaged (`cargo run --bin spike-a2dp`). If stage 1 and stage 2
//! both pass with no package identity, MSIX is off the table entirely.
//!
//! # Staging, and why it exists
//!
//! Stage 1 is read-only: it enumerates and constructs the connection object
//! but never advertises the PC as a sink, so a paired phone cannot latch on
//! and redirect its audio to this machine's default output.
//!
//! Stage 2 (`--open`) calls `StartAsync`, which DOES advertise this PC as an
//! available A2DP sink, and `OpenAsync`, which opens the audio stream. On a
//! machine with an already-paired phone that means remote audio can start
//! playing out of the default render endpoint without further warning. It is
//! therefore opt-in, never the default.

use anyhow::{Context, Result, bail};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Media::Audio::AudioPlaybackConnection;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

fn main() -> Result<()> {
    // WinRT activation needs an initialized apartment. MTA is correct for a
    // console process with no message pump.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx failed")?;

    let open = std::env::args().any(|a| a == "--open");

    println!("== stage 1: enumeration + construction (read-only) ==");

    // --- The selector -------------------------------------------------------
    // Reaching this call at all proves the WinRT class activates unpackaged.
    // If package identity were required merely to *activate* the class, this
    // is where it would fail.
    let selector = AudioPlaybackConnection::GetDeviceSelector()
        .context("GetDeviceSelector failed — class did not activate unpackaged")?;
    println!("selector: {selector}");

    // --- The devices --------------------------------------------------------
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

    let mut first_id = None;
    for device in &devices {
        let id = device.Id().context("device has no Id")?;
        let name = device.Name().context("device has no Name")?;
        println!("  - {name}\n      id: {id}");
        if first_id.is_none() {
            first_id = Some((id, name));
        }
    }

    // --- The capability gate ------------------------------------------------
    // This is the load-bearing call. `bluetooth` is a restricted
    // DeviceCapability; if it is enforced at construction time for an
    // unpackaged process, TryCreateFromId is where it shows up — either as an
    // access-denied HRESULT or as a null return.
    let (id, name) = first_id.expect("count > 0 was checked above");
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
                 required; confirm the capability is satisfied by an identity-only\n\
                 package before writing any more code."
            );
            return Ok(());
        }
    };

    println!("  device id round-trip: {}", connection.DeviceId()?);
    println!("  initial state: {:?}", connection.State()?);

    if !open {
        // Drop without ever advertising. Nothing on the network learned that
        // this PC is willing to be a speaker.
        drop(connection);
        println!(
            "\nstage 1 PASSED. Stage 2 (StartAsync/OpenAsync) skipped: it advertises\n\
             this PC as an A2DP sink and can route a paired phone's audio to the\n\
             default output. Re-run with --open when the machine's audio is free."
        );
        return Ok(());
    }

    // --- Stage 2: actually advertise and open -------------------------------
    println!("\n== stage 2: StartAsync + OpenAsync (WILL affect system audio) ==");

    connection
        .StartAsync()
        .context("StartAsync dispatch failed")?
        .join()
        .context("StartAsync did not complete — likely the capability gate")?;
    println!("StartAsync OK — PC is now advertising as an A2DP sink");

    let token = connection.StateChanged(&windows::Foundation::TypedEventHandler::<
        AudioPlaybackConnection,
        windows::core::IInspectable,
    >::new(|sender, _| {
        if let Some(s) = sender.as_ref() {
            println!("  [event] StateChanged -> {:?}", s.State()?);
        }
        Ok(())
    }))?;

    let result = connection
        .OpenAsync()
        .context("OpenAsync dispatch failed")?
        .join()
        .context("OpenAsync did not complete")?;
    println!("OpenAsync status: {:?}", result.Status()?);
    println!("state after open: {:?}", connection.State()?);

    println!("\nstreaming for 30s — play audio on the phone and listen. Ctrl-C to stop.");
    std::thread::sleep(std::time::Duration::from_secs(30));

    connection.RemoveStateChanged(token).ok();
    println!("done");
    Ok(())
}
