//! PurpToof - a Windows A2DP sink that notices when its own audio path has
//! died and restarts it.
//!
//! Still a skeleton. The tray-resident egui app grows here at milestone 8;
//! for now the binary exists to host `--debug-sessions`.

use anyhow::{Context, Result};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

mod debug_sessions;

fn main() -> Result<()> {
    // WinRT activation and the WASAPI interfaces both need an initialized
    // apartment. MTA is correct while there is no message pump; this moves
    // when the egui event loop arrives.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx failed")?;

    if std::env::args().any(|a| a == "--debug-sessions") {
        return debug_sessions::run();
    }

    println!("purptoof {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  --debug-sessions   dump WASAPI sessions and GSMTC state (read-only)");
    println!();
    println!("The tray app is not built yet. See CLAUDE.md for the order of work.");
    Ok(())
}
