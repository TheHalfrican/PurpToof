//! WinRT, COM and Win32, behind the traits in [`crate::core::traits`].
//!
//! **This layer is kept too dumb to be wrong in an interesting way.** The rule
//! from CLAUDE.md is that if a function contains a branch it belongs in
//! `core/`; what remains here is translation, and every translation that can be
//! subtly wrong gets exactly one named helper rather than being inlined at a
//! call site.
//!
//! That rule was written after `IAudioSessionControl2::IsSystemSoundsSession()`
//! made `.is_ok()` label all nine sessions as system sounds. It returns a raw
//! `HRESULT` where `S_OK` means yes and `S_FALSE` means *no*, and both are
//! success codes. See `docs/verify.md`.
//!
//! `platform/` is exempt from the test-ratio gate on purpose; padding tests
//! onto unmockable FFI is how coverage targets produce fake confidence. The
//! exceptions are the pure functions that sneak in here anyway, such as
//! [`a2dp_session::looks_like_a2dp_session`] and the status translations, which
//! are tested because they can be tested honestly.

pub mod a2dp_session;
pub mod autostart;
pub mod meter;
pub mod remote;
pub mod sink;
pub mod triggers;
pub mod worker;

pub use meter::{MeterReading, MeterScope, WasapiMeter};
pub use remote::GsmtcRemote;
pub use sink::{Sink, SinkDevice, list_devices};
pub use worker::{Command, LogEntry, Snapshot, Worker};
