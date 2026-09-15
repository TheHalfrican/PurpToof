//! Where the config and the logs live.
//!
//! Two modes, decided by whether a config file sits next to the executable:
//!
//! - **Portable.** `purptoof.toml` beside the exe wins. Drop the binary and a
//!   config in a folder and everything stays there, which is how the app is
//!   used before there is an installer and how anyone running it from a USB
//!   stick expects it to behave.
//! - **Installed.** Otherwise `%APPDATA%\PurpToof\`. An installed build lives
//!   in Program Files, which is not writable by a normal user, so writing
//!   settings next to the exe would fail the moment someone changed one.
//!
//! Logs sit under the same directory, so a user only has one place to look.
//!
//! Nothing here touches windows-rs - it is `std::env` and paths - so it stays
//! in `core/` and is testable.

use std::path::{Path, PathBuf};

/// The config file's name, in either mode.
pub const CONFIG_FILE: &str = "purptoof.toml";

/// Which of the two layouts is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A config file was found beside the executable.
    Portable,
    /// Per-user application data.
    Installed,
}

/// Resolved locations for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub mode: Mode,
    pub config: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    /// Work out where things go.
    ///
    /// Never fails: if neither the executable's directory nor `%APPDATA%` can
    /// be determined, it falls back to the current directory. A daemon that
    /// refuses to start because it could not find a settings file would be
    /// worse than one that runs on defaults.
    pub fn resolve() -> Self {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));

        if let Some(dir) = &beside {
            let candidate = dir.join(CONFIG_FILE);
            if candidate.is_file() {
                return Self::rooted(Mode::Portable, dir.clone());
            }
        }

        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|d| d.join("PurpToof"))
            .or(beside)
            .unwrap_or_else(|| PathBuf::from("."));

        Self::rooted(Mode::Installed, appdata)
    }

    fn rooted(mode: Mode, dir: PathBuf) -> Self {
        Self {
            mode,
            config: dir.join(CONFIG_FILE),
            log_dir: dir.join("logs"),
        }
    }

    /// Create the directories the config and logs need.
    ///
    /// Best effort: a read-only location should degrade to "runs on defaults,
    /// logs nowhere", not to a startup failure.
    pub fn ensure_dirs(&self) {
        if let Some(parent) = self.config.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(&self.log_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_sit_beside_the_config() {
        // One place to look. A user told "check the logs" should find them
        // next to the settings file they already know about.
        let p = Paths::resolve();
        assert_eq!(
            p.config.parent(),
            p.log_dir.parent(),
            "config and logs must share a root"
        );
    }

    #[test]
    fn the_config_is_named_consistently() {
        assert!(Paths::resolve().config.ends_with(CONFIG_FILE));
    }

    #[test]
    fn resolution_always_produces_something() {
        // The fallback chain must not be able to yield an empty path, or the
        // app fails to start on a machine with an unusual environment.
        let p = Paths::resolve();
        assert!(p.config.components().count() > 0);
        assert!(p.log_dir.components().count() > 0);
    }
}
