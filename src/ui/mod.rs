//! The egui front end.
//!
//! Thin by rule. Everything that decides anything lives in `core/`; this
//! module reads [`crate::platform::Snapshot`] and draws it.
//!
//! # What this session's measurements demand of the UI
//!
//! The view is not a generic status panel. Three findings shape it:
//!
//! 1. **`Degraded` is the normal state on the target hardware.** The iPhone
//!    publishes no GSMTC session, so Signal B is permanently unavailable. If
//!    that reads as an error the app looks broken while working perfectly, so
//!    it is stated once, quietly, as a fact about the device.
//! 2. **A pause is indistinguishable from a dead audio path.** The state
//!    machine provably cannot tell them apart without Signal B, which makes
//!    the *user* the discriminator - they know whether they pressed pause.
//!    The meter is therefore the primary instrument and Reconnect has to be
//!    one obvious click, not a menu item.
//! 3. **The meter can be masked.** When no A2DP session is attributable the
//!    reading falls back to the endpoint, where another application's audio
//!    can sit above `silence_eps` while A2DP is silent - observed at 0.0626
//!    during a measured run. When that happens the meter must say so rather
//!    than quietly lie.

mod app;
mod icon;
mod tray;

pub use app::PurpToofApp;
pub use icon::icon_data;
