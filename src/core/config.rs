//! Configuration, and the tunables the health state machine reads.
//!
//! Deserialisation rules, both deliberate:
//!
//! - **Every field has a default.** A missing key is never fatal. A config
//!   file that predates a new setting keeps working.
//! - **Unknown keys are ignored, not fatal.** There is no
//!   `deny_unknown_fields`. Downgrading a build, or a hand-edited file with a
//!   typo, must not leave the app unable to start - it is a background audio
//!   utility, and refusing to run is a worse outcome than ignoring a line.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

fn default_silence_timeout_ms() -> u64 {
    3_000
}

fn default_degraded_silence_timeout_ms() -> u64 {
    // Deliberately much longer than the normal timeout. Without Signal B we
    // cannot tell a dead stream from a quiet passage, so the only safe move is
    // to wait long enough that no piece of music could plausibly be this
    // quiet for this long.
    15_000
}

fn default_silence_eps() -> f32 {
    0.000_5
}

fn default_autostart() -> bool {
    true
}

fn default_start_minimized() -> bool {
    // CLAUDE.md's example config shows `start_minimized = true`. Changed on
    // the user's instruction after using it: launching straight to the tray
    // gives no confirmation the app came up at all, and the first thing anyone
    // wants after starting it is to see whether the link is live. Still
    // available as an option for people who autostart it.
    false
}

fn default_close_to_tray() -> bool {
    true
}

fn default_rearm_min_interval_ms() -> u64 {
    1_000
}

fn default_rearm_escalation_threshold() -> u32 {
    5
}

fn default_trigger_debounce_ms() -> u64 {
    // Resume in particular fires a burst of overlapping events.
    750
}

fn default_open_transition_timeout_ms() -> u64 {
    // How long to wait for StateChanged -> Opened after open() returns
    // success. Success followed by no transition is a failed recovery.
    5_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Preferred device, persisted between runs. `None` means "pick the only
    /// paired device, or ask".
    #[serde(default)]
    pub device_id: Option<String>,

    #[serde(default = "default_silence_timeout_ms")]
    pub silence_timeout_ms: u64,

    /// Used instead of `silence_timeout_ms` when Signal B is unavailable.
    #[serde(default = "default_degraded_silence_timeout_ms")]
    pub degraded_silence_timeout_ms: u64,

    #[serde(default = "default_silence_eps")]
    pub silence_eps: f32,

    /// Whether to auto-recover when Signal B is unavailable.
    ///
    /// Defaults to **false**, the conservative reading. Without the remote's
    /// own view there is no way to distinguish a dead stream from a quiet
    /// passage, and reconnecting during a quiet intro is worse than the
    /// original bug. With this off the app degrades to a longer timeout plus
    /// an explicit user-triggered reconnect, and says so in the UI.
    #[serde(default)]
    pub recover_without_remote_signal: bool,

    /// Floor on how often a benign re-arm may fire. Re-arms skip the backoff
    /// ladder by design, so without this a link that closes instantly on open
    /// would reopen in a tight loop.
    #[serde(default = "default_rearm_min_interval_ms")]
    pub rearm_min_interval_ms: u64,

    /// Consecutive re-arms that fail to produce healthy flow before
    /// escalating to a real recovery, so the ladder takes over.
    #[serde(default = "default_rearm_escalation_threshold")]
    pub rearm_escalation_threshold: u32,

    #[serde(default = "default_trigger_debounce_ms")]
    pub trigger_debounce_ms: u64,

    #[serde(default = "default_open_transition_timeout_ms")]
    pub open_transition_timeout_ms: u64,

    #[serde(default = "default_autostart")]
    pub autostart: bool,

    #[serde(default = "default_start_minimized")]
    pub start_minimized: bool,

    /// Whether the window's close button hides to the tray instead of exiting.
    ///
    /// Defaults to true: this is a background service with a window attached,
    /// not a document editor. Closing the window when the whole point is to
    /// keep the audio path alive is almost never what the user meant - and if
    /// it were, the app would stop recovering the moment they tidied their
    /// desktop. Quit lives in the tray menu for the times they do mean it.
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
}

impl Default for Config {
    fn default() -> Self {
        // Written as an empty-table deserialise rather than by repeating the
        // literals, so `Default` and "every key missing" can never drift
        // apart. The test below pins that they agree.
        toml::from_str("").expect("empty config must deserialise to defaults")
    }
}

impl Config {
    pub fn silence_timeout(&self) -> Duration {
        Duration::from_millis(self.silence_timeout_ms)
    }

    pub fn degraded_silence_timeout(&self) -> Duration {
        Duration::from_millis(self.degraded_silence_timeout_ms)
    }

    pub fn rearm_min_interval(&self) -> Duration {
        Duration::from_millis(self.rearm_min_interval_ms)
    }

    pub fn trigger_debounce(&self) -> Duration {
        Duration::from_millis(self.trigger_debounce_ms)
    }

    pub fn open_transition_timeout(&self) -> Duration {
        Duration::from_millis(self.open_transition_timeout_ms)
    }

    /// Which silence timeout applies, given whether Signal B is available.
    pub fn effective_silence_timeout(&self, remote_available: bool) -> Duration {
        if remote_available {
            self.silence_timeout()
        } else {
            self.degraded_silence_timeout()
        }
    }
}

impl Config {
    /// Read the config, falling back to defaults.
    ///
    /// Returns the config plus a description of anything that went wrong, so
    /// the caller can surface it. A malformed file must NOT stop the app: the
    /// whole point is to keep audio alive, and refusing to start because of a
    /// stray character in a settings file would be the opposite of that.
    /// Unknown keys are already ignored by serde, so this only trips on real
    /// syntax or type errors.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // Absent is the normal first-run case, not a problem worth
            // reporting.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), None);
            }
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("could not read {}: {e}", path.display())),
                );
            }
        };

        match toml::from_str(&text) {
            Ok(c) => (c, None),
            Err(e) => (
                Self::default(),
                Some(format!(
                    "{} is malformed, using defaults: {e}",
                    path.display()
                )),
            ),
        }
    }

    /// Write the config back.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_all_defaults() {
        let c: Config = toml::from_str("").expect("empty must parse");
        assert_eq!(c.device_id, None);
        assert_eq!(c.silence_timeout_ms, 3_000);
        assert_eq!(c.degraded_silence_timeout_ms, 15_000);
        assert_eq!(c.silence_eps, 0.000_5);
        assert!(!c.recover_without_remote_signal);
        assert_eq!(c.rearm_min_interval_ms, 1_000);
        assert_eq!(c.rearm_escalation_threshold, 5);
        assert_eq!(c.trigger_debounce_ms, 750);
        assert_eq!(c.open_transition_timeout_ms, 5_000);
        assert!(c.autostart);
        assert!(
            !c.start_minimized,
            "starting hidden gives no sign it worked"
        );
        // Close-to-tray is on by default: this is a background service with a
        // window attached, and closing the window would stop it recovering.
        assert!(c.close_to_tray);
    }

    #[test]
    fn default_impl_agrees_with_empty_file() {
        let from_default = Config::default();
        let from_empty: Config = toml::from_str("").unwrap();
        assert_eq!(from_default, from_empty);
    }

    #[test]
    fn the_documented_config_shape_parses() {
        // Exactly the example from CLAUDE.md.
        let src = r#"
            device_id = "BluetoothLE#..."
            silence_timeout_ms = 3000
            silence_eps = 0.0005
            autostart = true
            start_minimized = true
        "#;
        let c: Config = toml::from_str(src).expect("documented shape must parse");
        assert_eq!(c.device_id.as_deref(), Some("BluetoothLE#..."));
        assert_eq!(c.silence_timeout_ms, 3_000);
        assert_eq!(c.silence_eps, 0.000_5);
        assert!(c.autostart);
        assert!(c.start_minimized);
    }

    #[test]
    fn each_key_can_be_set_independently_of_the_others() {
        // The point: setting one key must not disturb the defaults of any
        // other. A single `#[serde(default)]` missing from a field would show
        // up here.
        let c: Config = toml::from_str("silence_timeout_ms = 9999").unwrap();
        assert_eq!(c.silence_timeout_ms, 9_999);
        assert_eq!(c.silence_eps, 0.000_5, "other defaults must survive");
        assert!(c.autostart);

        let c: Config = toml::from_str("autostart = false").unwrap();
        assert!(!c.autostart);
        assert_eq!(c.silence_timeout_ms, 3_000);
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_fatal() {
        // A downgraded build, or a typo, must not stop the app starting.
        let src = r#"
            silence_timeout_ms = 4000
            some_setting_from_a_newer_build = "whatever"
            typoed_kee = 17
        "#;
        let c: Config = toml::from_str(src).expect("unknown keys must not be fatal");
        assert_eq!(c.silence_timeout_ms, 4_000);
    }

    #[test]
    fn round_trips_through_toml() {
        let original = Config {
            device_id: Some("some-device-id".into()),
            silence_timeout_ms: 2_500,
            degraded_silence_timeout_ms: 20_000,
            silence_eps: 0.001,
            recover_without_remote_signal: true,
            rearm_min_interval_ms: 1_500,
            rearm_escalation_threshold: 3,
            trigger_debounce_ms: 500,
            open_transition_timeout_ms: 4_000,
            autostart: false,
            start_minimized: false,
            close_to_tray: false,
        };
        let text = toml::to_string(&original).expect("must serialise");
        let back: Config = toml::from_str(&text).expect("must round-trip");
        assert_eq!(original, back);
    }

    #[test]
    fn round_trip_of_defaults_is_stable() {
        let c = Config::default();
        let text = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn absent_device_id_round_trips_as_absent() {
        let c = Config::default();
        assert_eq!(c.device_id, None);
        let text = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.device_id, None);
    }

    #[test]
    fn degraded_timeout_is_selected_when_signal_b_is_missing() {
        let c = Config::default();
        assert_eq!(c.effective_silence_timeout(true), Duration::from_secs(3));
        assert_eq!(c.effective_silence_timeout(false), Duration::from_secs(15));
    }

    #[test]
    fn degraded_timeout_is_longer_than_the_normal_one() {
        // If this inverts, the degraded path becomes trigger-happy in exactly
        // the situation where we have the least information.
        let c = Config::default();
        assert!(
            c.degraded_silence_timeout() > c.silence_timeout(),
            "degraded timeout must be more patient, not less"
        );
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("purptoof-cfg-{name}-{}.toml", std::process::id()));
        p
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        // First run. Reporting this would train the user to ignore warnings.
        let (c, err) = Config::load(&temp("absent"));
        assert_eq!(c, Config::default());
        assert!(err.is_none(), "absent config should be silent: {err:?}");
    }

    #[test]
    fn a_malformed_file_falls_back_and_says_so() {
        // The app must still come up - refusing to start over a settings typo
        // is the opposite of keeping the audio alive.
        let p = temp("malformed");
        std::fs::write(&p, "this is not = = toml").unwrap();
        let (c, err) = Config::load(&p);
        assert_eq!(c, Config::default());
        assert!(err.is_some(), "a broken file must be reported");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn it_round_trips_through_a_real_file() {
        let p = temp("roundtrip");
        let original = Config {
            silence_timeout_ms: 4_321,
            close_to_tray: false,
            ..Default::default()
        };
        original.save(&p).expect("must save");

        let (back, err) = Config::load(&p);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(back, original);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_partial_file_keeps_defaults_for_everything_else() {
        // Hand-edited configs are normal; someone setting one key must not
        // silently zero the rest.
        let p = temp("partial");
        std::fs::write(&p, "silence_timeout_ms = 9999\n").unwrap();
        let (c, err) = Config::load(&p);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(c.silence_timeout_ms, 9_999);
        assert_eq!(c.silence_eps, Config::default().silence_eps);
        assert_eq!(c.close_to_tray, Config::default().close_to_tray);
        let _ = std::fs::remove_file(&p);
    }
}
