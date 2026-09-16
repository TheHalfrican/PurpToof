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
pub mod btaddr;
pub mod config;
pub mod health;
pub mod paths;
pub mod power;
pub mod supervisor;
pub mod traits;
pub mod types;

#[cfg(test)]
pub mod fakes;

pub use btaddr::{BtAddr, address_from_interface_id};
pub use config::Config;
pub use health::HealthMonitor;
pub use paths::{Mode, Paths};
pub use power::{RadioPowerPolicy, classify as classify_radio_power, devnode_id_from_interface_id};
pub use supervisor::{Supervisor, Tick};
pub use traits::{AudioMeter, Clock, RemotePlayback, SinkConnection, SinkError, SystemClock};
pub use types::{
    Action, HealthStatus, LinkState, Observation, PlaybackStatus, ReArmReason, RecoveryOutcome,
    RecoveryReason, Trigger,
};
