//! PurpToof - a Windows A2DP sink that notices when its own audio path has
//! died and restarts it.
//!
//! Still a skeleton. The tray-resident egui app grows here at milestone 8;
//! for now the binary exists to host `--debug-sessions`.

use anyhow::{Context, Result};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

// The pure logic lives in the library half of this crate (src/lib.rs).

mod debug_sessions;
mod run;

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

    // The real app, headless. Unlike the two above this is NOT read-only: it
    // advertises the PC as a sink and routes phone audio to the default
    // output, so it is opt-in and bounded.
    if let Some(secs) = run_secs(std::env::args()) {
        return run::run(secs);
    }

    println!("purptoof {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  --debug-sessions   dump WASAPI sessions and GSMTC state (read-only)");
    println!("  --watch[=SECS]     the same, sampled once a second (default 120)");
    println!("  --run[=SECS]       the real supervisor, headless (default 300).");
    println!("                     ROUTES PHONE AUDIO to the default output.");
    println!();
    println!("The tray app is not built yet. See CLAUDE.md for the order of work.");
    Ok(())
}

/// Seconds `--run` should run for, or `None` if the flag is absent.
fn run_secs(args: impl Iterator<Item = String>) -> Option<u32> {
    flag_secs(args, "--run", 300)
}

/// Seconds `--watch` should run for, or `None` if the flag is absent.
fn watch_secs(args: impl Iterator<Item = String>) -> Option<u32> {
    flag_secs(args, "--watch", 120)
}

/// Seconds for a `--flag`, `--flag=SECS` or `--flag SECS` duration argument.
///
/// A malformed value falls back to the default rather than aborting. These are
/// runs on real hardware with a phone in hand; refusing to start over a typo
/// wastes the setup, and every one of them is time-bounded anyway.
fn flag_secs(args: impl Iterator<Item = String>, flag: &str, default: u32) -> Option<u32> {
    let eq = format!("{flag}=");
    let mut rest = args.skip_while(|a| a != flag && !a.starts_with(&eq));
    let found = rest.next()?;
    match found.strip_prefix(&eq) {
        Some(v) => Some(v.parse().unwrap_or(default)),
        None => Some(rest.next().and_then(|v| v.parse().ok()).unwrap_or(default)),
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
    fn the_two_duration_flags_do_not_match_each_other() {
        // Both flags share one parser, so a prefix match would make --run=60
        // silently start a watch. The distinct mode matters: one is read-only
        // and one routes the phone audio.
        assert_eq!(secs(&["purptoof", "--run=60"]), None);
        assert_eq!(secs(&["purptoof", "--run", "60"]), None);
        assert_eq!(
            super::run_secs(["purptoof", "--watch=60"].iter().map(|s| s.to_string())),
            None
        );
        assert_eq!(
            super::run_secs(["purptoof", "--run=60"].iter().map(|s| s.to_string())),
            Some(60)
        );
    }

    #[test]
    fn a_longer_flag_with_the_same_prefix_is_not_matched() {
        assert_eq!(secs(&["purptoof", "--watchdog"]), None);
    }

    #[test]
    fn malformed_value_falls_back_rather_than_aborting() {
        assert_eq!(secs(&["purptoof", "--watch=abc"]), Some(120));
        assert_eq!(secs(&["purptoof", "--watch", "abc"]), Some(120));
    }
}
