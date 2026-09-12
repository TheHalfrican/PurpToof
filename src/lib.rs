//! PurpToof - a Windows A2DP sink that notices when its own audio path has
//! died and restarts it.
//!
//! Split as a library plus a thin binary. That is not a change of plan from
//! "single binary" - one `purptoof.exe` still ships - it is what makes the
//! layering enforceable:
//!
//! - [`core`] is pure logic with **zero** windows-rs imports, and is where
//!   every branch lives. Being a library means its items are public API
//!   rather than dead code waiting to be wired up, so the lint gate stays
//!   meaningful instead of needing a blanket `allow`.
//! - `platform/` (milestone 6) implements [`core::traits`] against WinRT, COM
//!   and Win32, and is kept too thin to be wrong in an interesting way.
//! - `ui/` (milestone 8) is egui, and also thin.

pub mod core;
