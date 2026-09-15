//! `--run`: the real thing, headless.
//!
//! Starts the supervisor on its own MTA thread and renders its snapshots from
//! this one. That is deliberately the shape milestone 8 will use - egui polls
//! [`Worker::snapshot`] every frame and posts `Trigger::Manual` when Reconnect
//! is pressed - so the channel boundary gets exercised on real hardware before
//! any UI depends on it.
//!
//! **This advertises the PC as an A2DP sink and routes phone audio to the
//! default output**, so it is opt-in and takes a duration rather than running
//! forever by default.

use std::time::{Duration, Instant};

use anyhow::Result;

use purptoof::core::{Config, HealthStatus};
use purptoof::platform::Worker;
use purptoof::platform::meter::MeterScope;

/// How often to re-render. The supervisor ticks at 10 Hz on its own thread;
/// this is only the reader.
const RENDER: Duration = Duration::from_millis(100);

pub fn run(secs: u32, config: Config) -> Result<()> {
    let worker = Worker::spawn(config)?;

    let first = worker.snapshot();
    println!("PurpToof --run, {secs}s");
    println!("  device: {}", first.device_name);
    println!(
        "  triggers: {}/3 event-driven registered (radio one is unconfirmed); default-device watch {}",
        first.triggers_registered,
        if first.device_watch_active {
            "active"
        } else {
            "UNAVAILABLE"
        }
    );
    println!();
    println!("The PC is now advertising as an A2DP sink and will keep itself");
    println!("armed. Route audio to this PC from Control Center - not Settings >");
    println!("Bluetooth, which produces a link that drops on idle.");
    println!();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs as u64);
    let mut last_status: Option<HealthStatus> = None;
    let mut last_scope: Option<MeterScope> = None;
    let mut logged = 0usize;
    let mut rearms = 0u64;

    while Instant::now() < deadline {
        if !worker.is_running() {
            println!("\nsupervisor thread stopped");
            break;
        }

        let snap = worker.snapshot();
        let at = start.elapsed().as_secs_f32();

        // Only speak when something changes. A line per tick at 10 Hz is noise,
        // and the status line is supposed to be honest, not frequent.
        if last_status != Some(snap.status) {
            println!("[{at:>6.1}s] {}", describe(snap.status));
            last_status = Some(snap.status);
        }

        // Meter provenance is worth surfacing: an endpoint-scoped reading can
        // be masked by other applications on this PC, and the user deserves to
        // know when the meter is less trustworthy than usual.
        if last_scope != Some(snap.scope) {
            println!("[{at:>6.1}s] meter scope: {}", describe_scope(snap.scope));
            last_scope = Some(snap.scope);
        }

        // Re-arms are benign and stay out of the reconnect log, but printing
        // them here is the only way to see a trigger actually fire.
        if snap.rearms > rearms {
            println!(
                "[{at:>6.1}s] re-arm x{} - {}",
                snap.rearms - rearms,
                snap.last_rearm.as_deref().unwrap_or("?")
            );
            rearms = snap.rearms;
        }

        if snap.log_len > logged {
            for entry in worker.log().into_iter().skip(logged) {
                println!("[{at:>6.1}s] RECONNECT: {}", entry.reason);
            }
            logged = snap.log_len;
        }

        std::thread::sleep(RENDER);
    }

    let events = worker.snapshot().log_len;
    // Dropping the worker sends Shutdown and joins the thread.
    drop(worker);

    println!("\nrun complete, {events} recovery event(s) logged");
    Ok(())
}

fn describe(s: HealthStatus) -> String {
    match s {
        HealthStatus::Disconnected => "Disconnected".into(),
        HealthStatus::Listening => "Listening - advertising, waiting for a device".into(),
        HealthStatus::Streaming => "Streaming".into(),
        HealthStatus::ConnectedSilent => "Connected, silent".into(),
        HealthStatus::Degraded => "Connected, silent (degraded - no AVRCP signal)".into(),
        HealthStatus::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})"),
    }
}

fn describe_scope(s: MeterScope) -> &'static str {
    match s {
        MeterScope::Session => "A2DP session (trustworthy)",
        MeterScope::Endpoint => "endpoint fallback - MASKABLE by other apps on this PC",
        MeterScope::SessionGone => "A2DP session vanished - reporting silence, not falling back",
    }
}
