//! PurpToof — a Windows A2DP sink that notices when its own audio path has
//! died and restarts it.
//!
//! Skeleton only at this milestone. The real entry point grows here once the
//! packaging question (unpackaged Win32 vs sparse MSIX) is settled by
//! `src/bin/spike-a2dp.rs`.

fn main() {
    println!("purptoof {}", env!("CARGO_PKG_VERSION"));
    println!("skeleton build — see `cargo run --bin spike-a2dp` for the packaging spike");
}
