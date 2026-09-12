//! Pure logic. **Zero windows-rs imports, by rule.**
//!
//! Everything that branches lives here, behind the traits in
//! [`traits`]. `platform/` implements those traits against WinRT, COM and
//! Win32 and is kept so thin it cannot be wrong in an interesting way; tests
//! implement them as fakes and drive the state machine on a fake clock.
//!
//! The 0.9:1 test-to-production ratio is enforced on this directory only.
//! `platform/` and `ui/` are exempt on purpose - see
//! `scripts/check-test-ratio.ps1` for why.

pub mod backoff;
pub mod config;
pub mod health;
pub mod traits;
pub mod types;

#[cfg(test)]
pub mod fakes;

pub use config::Config;
pub use health::HealthMonitor;
pub use traits::{AudioMeter, Clock, RemotePlayback, SinkConnection, SinkError, SystemClock};
pub use types::{
    Action, HealthStatus, LinkState, Observation, PlaybackStatus, ReArmReason, RecoveryOutcome,
    RecoveryReason, Trigger,
};
