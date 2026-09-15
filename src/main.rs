//! PurpToof - a Windows A2DP sink that notices when its own audio path has
//! died and restarts it.
//!
//! Still a skeleton. The tray-resident egui app grows here at milestone 8;
//! for now the binary exists to host `--debug-sessions`.

use anyhow::{Context, Result};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

// The pure logic lives in the library half of this crate (src/lib.rs).

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

    // `--watch[=SECS]` is the same observations as `--debug-sessions`, sampled
    // once a second for a while instead of once. It exists because the useful
    // questions are about transitions - pause, resume, disconnect - and a
    // one-shot dump needs the operator to hold the phone and the keyboard at
    // the same moment, which is how the first hardware run produced a window
    // of zeroes.
    if let Some(secs) = watch_secs(std::env::args()) {
        return debug_sessions::watch(secs);
    }

    println!("purptoof {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  --debug-sessions   dump WASAPI sessions and GSMTC state (read-only)");
    println!("  --watch[=SECS]     the same, sampled once a second (default 120)");
    println!();
    println!("The tray app is not built yet. See CLAUDE.md for the order of work.");
    Ok(())
}

/// Seconds `--watch` should run for, or `None` if the flag is absent.
///
/// Accepts `--watch`, `--watch=SECS` and `--watch SECS`. A malformed value
/// falls back to the default rather than aborting - this is a diagnostic run
/// on real hardware, and refusing to start over a typo wastes the setup.
fn watch_secs(args: impl Iterator<Item = String>) -> Option<u32> {
    const DEFAULT: u32 = 120;
    let mut rest = args.skip_while(|a| !a.starts_with("--watch"));
    let flag = rest.next()?;
    match flag.strip_prefix("--watch=") {
        Some(v) => Some(v.parse().unwrap_or(DEFAULT)),
        None => Some(rest.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT)),
    }
}

#[cfg(test)]
mod tests {
    use super::watch_secs;

    fn secs(args: &[&str]) -> Option<u32> {
        watch_secs(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn absent_flag_means_no_watch() {
        assert_eq!(secs(&["purptoof"]), None);
        assert_eq!(secs(&["purptoof", "--debug-sessions"]), None);
    }

    #[test]
    fn bare_flag_takes_the_default() {
        assert_eq!(secs(&["purptoof", "--watch"]), Some(120));
    }

    #[test]
    fn both_spellings_parse() {
        assert_eq!(secs(&["purptoof", "--watch=45"]), Some(45));
        assert_eq!(secs(&["purptoof", "--watch", "45"]), Some(45));
    }

    #[test]
    fn malformed_value_falls_back_rather_than_aborting() {
        assert_eq!(secs(&["purptoof", "--watch=abc"]), Some(120));
        assert_eq!(secs(&["purptoof", "--watch", "abc"]), Some(120));
    }
}
