//! `--run`: the real thing, headless.
//!
//! Wires `platform/`'s implementations into [`Supervisor`] and ticks it at
//! 10 Hz. No UI yet - that is milestone 8 - but this is the actual app: the
//! same state machine, the same always-armed sink, the same session-scoped
//! meter that will sit behind the tray icon.
//!
//! **This advertises the PC as an A2DP sink and routes phone audio to the
//! default output**, so it is opt-in and takes a duration rather than running
//! forever by default.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use purptoof::core::{Config, HealthStatus, Supervisor, SystemClock, Tick};
use purptoof::platform::{GsmtcRemote, Sink, WasapiMeter, meter::MeterScope};

/// The poll rate the whole design assumes: fast enough that a dead path is
/// noticed within the silence timeout, slow enough to cost nothing.
const POLL: Duration = Duration::from_millis(100);

pub fn run(secs: u32) -> Result<()> {
    // SAFETY: main() initialized COM on this thread before dispatching here.
    let sink = unsafe { Sink::connect_first() }.context("could not construct the sink")?;
    let device = sink.device().clone();
    let meter = unsafe { WasapiMeter::new() }.context("could not build the meter")?;
    let remote = unsafe { GsmtcRemote::new() }.context("could not reach GSMTC")?;

    println!("PurpToof --run, {secs}s");
    println!("  device: {}", device.name);
    println!();
    println!("The PC is now advertising as an A2DP sink and will keep itself");
    println!("armed. Route audio to this PC from Control Center (not Settings >");
    println!("Bluetooth - that path produces a link that drops on idle).");
    println!();

    let mut sup = Supervisor::new(SystemClock, Config::default(), sink, meter, remote);

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs as u64);
    let mut last_status: Option<HealthStatus> = None;
    let mut last_scope: Option<MeterScope> = None;

    while Instant::now() < deadline {
        let tick = sup.tick();

        // Only speak when something changes. A line per tick at 10 Hz is
        // noise, and the point of the status line is that it is honest, not
        // that it is frequent.
        let status = sup.status();
        if last_status != Some(status) {
            println!(
                "[{:>6.1}s] {}",
                start.elapsed().as_secs_f32(),
                describe(status)
            );
            last_status = Some(status);
        }

        // Meter provenance matters enough to surface: an endpoint-scoped
        // reading can be masked by other applications on this PC, and the user
        // deserves to know the meter is less trustworthy than usual.
        let scope = sup.meter().read().scope;
        if last_scope != Some(scope) {
            println!(
                "[{:>6.1}s] meter scope: {}",
                start.elapsed().as_secs_f32(),
                describe_scope(scope)
            );
            last_scope = Some(scope);
        }

        match tick {
            Tick::Dispatched(action) if action.is_loggable() => {
                println!(
                    "[{:>6.1}s] RECONNECT: {action:?}",
                    start.elapsed().as_secs_f32()
                );
            }
            Tick::Resolved(outcome) => {
                println!(
                    "[{:>6.1}s] open resolved: {outcome:?}",
                    start.elapsed().as_secs_f32()
                );
            }
            _ => {}
        }

        std::thread::sleep(POLL);
    }

    println!("\nrun complete");
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
