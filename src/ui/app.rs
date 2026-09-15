//! The single view.

use std::time::Duration;

use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};

use crate::core::{Config, HealthStatus, Trigger};
use crate::platform::meter::MeterScope;
use crate::platform::worker::{LogEntry, PEAK_HISTORY};
use crate::platform::{Snapshot, Worker};

/// Redraw cadence. Matches the supervisor's 10 Hz tick - drawing faster would
/// only re-render identical samples and keep a tray-resident app busy for
/// nothing.
const REPAINT: Duration = Duration::from_millis(100);

// Dark palette. Compact, no decorative chrome, per CLAUDE.md.
const BG_METER: Color32 = Color32::from_rgb(24, 24, 28);
const GRID: Color32 = Color32::from_rgb(48, 48, 54);
const GREEN: Color32 = Color32::from_rgb(120, 200, 120);
const AMBER: Color32 = Color32::from_rgb(220, 180, 90);
const BLUE: Color32 = Color32::from_rgb(120, 170, 220);
const RED: Color32 = Color32::from_rgb(220, 110, 110);
const DIM: Color32 = Color32::from_rgb(140, 140, 148);

pub struct PurpToofApp {
    /// `Err` when the supervisor could not start - most often no paired
    /// device. Shown as an explanation rather than crashing, because that is
    /// exactly the state a first-time user with nothing paired lands in.
    worker: Result<Worker, String>,
    log: Vec<LogEntry>,
    log_len: usize,
    eps: f32,
}

impl PurpToofApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let eps = config.silence_eps;
        let worker = Worker::spawn(config).map_err(|e| format!("{e:#}"));
        Self {
            worker,
            log: Vec::new(),
            log_len: 0,
            eps,
        }
    }
}

impl eframe::App for PurpToofApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The supervisor ticks on its own thread, so nothing here drives it -
        // this only asks egui to come back and read the next snapshot.
        ui.ctx().request_repaint_after(REPAINT);

        let worker = match &self.worker {
            Ok(w) => w,
            Err(e) => {
                let message = e.clone();
                egui::Frame::central_panel(ui.style()).show(ui, |ui| startup_error(ui, &message));
                return;
            }
        };

        let snap = worker.snapshot();

        // Only clone the log when it has actually changed; it carries owned
        // strings and this runs at 10 Hz.
        if snap.log_len != self.log_len {
            self.log = worker.log();
            self.log_len = snap.log_len;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;

            device_row(ui, &snap);
            ui.add_space(2.0);
            meter(ui, &snap, self.eps);
            status_line(ui, &snap);

            if ui
                .add_sized(
                    [ui.available_width(), 30.0],
                    egui::Button::new(RichText::new("Reconnect").size(14.0)),
                )
                .on_hover_text(
                    "Tears the connection down and rebuilds it. Safe at any time - \
                     and the intended fix when audio has died but the link still \
                     claims to be open.",
                )
                .clicked()
            {
                worker.trigger(Trigger::Manual);
            }

            ui.separator();

            // Footnotes first, laid out bottom-up, so the log can then expand
            // into whatever height is left rather than leaving dead space.
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                footnotes(ui, &snap);
                ui.separator();
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    reconnect_log(ui, &self.log);
                });
            });
        });
    }
}

fn startup_error(ui: &mut egui::Ui, message: &str) {
    ui.add_space(24.0);
    ui.label(RichText::new("Could not start").size(18.0).color(RED));
    ui.add_space(8.0);
    ui.label(RichText::new(message).color(DIM));
    ui.add_space(16.0);
    ui.label(
        RichText::new(
            "The most common cause is that no phone is paired with this PC yet. \
             Pair one in Windows Settings > Bluetooth & devices, then restart \
             PurpToof.",
        )
        .color(DIM),
    );
}

fn device_row(ui: &mut egui::Ui, snap: &Snapshot) {
    ui.horizontal(|ui| {
        let name = if snap.device_name.is_empty() {
            "(no device)"
        } else {
            &snap.device_name
        };
        ui.label(RichText::new(name).size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (text, color) = link_chip(snap.status);
            ui.label(RichText::new(text).color(color).size(12.0));
        });
    });
}

fn link_chip(status: HealthStatus) -> (&'static str, Color32) {
    match status {
        HealthStatus::Streaming => ("link open", GREEN),
        HealthStatus::ConnectedSilent | HealthStatus::Degraded => ("link open", DIM),
        HealthStatus::Listening => ("advertising", BLUE),
        HealthStatus::Reconnecting { .. } => ("reopening", AMBER),
        HealthStatus::Disconnected => ("no link", RED),
    }
}

/// Current level plus a rolling history.
///
/// The history is the point. Because the app cannot distinguish a pause from a
/// dead path, the useful question is not "is it silent" but "*when* did it go
/// silent" - and a single bar cannot answer that.
fn meter(ui: &mut egui::Ui, snap: &Snapshot, eps: f32) {
    let trustworthy = snap.scope == MeterScope::Session;
    let level_color = if !trustworthy {
        AMBER
    } else if snap.peak >= eps {
        GREEN
    } else {
        DIM
    };

    // --- current level ------------------------------------------------------
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, BG_METER);

    // Square-root scaling: peaks live in the low end of 0..1 and a linear bar
    // spends most of its width empty.
    let filled = snap.peak.clamp(0.0, 1.0).sqrt();
    if filled > 0.0 {
        let mut bar = rect;
        bar.set_width(rect.width() * filled);
        painter.rect_filled(bar, 2.0, level_color);
    }
    painter.text(
        rect.right_center() - egui::vec2(6.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{:.4}", snap.peak),
        egui::FontId::monospace(11.0),
        Color32::from_rgb(200, 200, 205),
    );

    // --- history ------------------------------------------------------------
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, BG_METER);

    // The silence threshold, drawn so the user can see what the watchdog sees
    // rather than guessing where "silent" begins.
    let eps_y = rect.bottom() - rect.height() * eps.sqrt().max(0.01);
    painter.line_segment(
        [
            egui::pos2(rect.left(), eps_y),
            egui::pos2(rect.right(), eps_y),
        ],
        Stroke::new(1.0, GRID),
    );

    let slot = rect.width() / PEAK_HISTORY as f32;
    for (i, sample) in snap.peak_history.iter().enumerate() {
        let h = sample.clamp(0.0, 1.0).sqrt() * rect.height();
        if h <= 0.0 {
            continue;
        }
        let x = rect.left() + i as f32 * slot;
        let color = if *sample >= eps { level_color } else { GRID };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, rect.bottom() - h),
                egui::pos2(x + (slot - 0.5).max(0.5), rect.bottom()),
            ),
            0.0,
            color,
        );
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new("15s ago").size(10.0).color(DIM));
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| match snap.silent_for {
                Some(d) if d.as_secs_f32() >= 1.0 => {
                    ui.label(
                        RichText::new(format!("silent for {:.0}s", d.as_secs_f32()))
                            .size(10.0)
                            .color(AMBER),
                    );
                }
                _ => {
                    ui.label(RichText::new("now").size(10.0).color(DIM));
                }
            },
        );
    });
}

fn status_line(ui: &mut egui::Ui, snap: &Snapshot) {
    let (text, color) = match snap.status {
        HealthStatus::Streaming => ("Streaming".to_string(), GREEN),
        HealthStatus::ConnectedSilent => ("Connected, silent".to_string(), DIM),
        HealthStatus::Degraded => ("Connected, silent".to_string(), DIM),
        HealthStatus::Listening => ("Waiting for a device".to_string(), BLUE),
        HealthStatus::Reconnecting { attempt } => {
            (format!("Reconnecting (attempt {attempt})"), AMBER)
        }
        HealthStatus::Disconnected => ("Disconnected".to_string(), RED),
    };
    ui.label(RichText::new(text).size(20.0).color(color).strong());
}

fn reconnect_log(ui: &mut egui::Ui, log: &[LogEntry]) {
    ui.label(RichText::new("Reconnect log").size(12.0).color(DIM));

    if log.is_empty() {
        // Phrased as reassurance rather than absence: an empty log is the good
        // outcome, and "no entries" reads like something is missing.
        ui.label(
            RichText::new("Nothing yet - it has not needed to recover.")
                .size(11.0)
                .color(DIM),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in log {
                ui.label(
                    RichText::new(format!("{}  {}", stamp(entry), entry.reason))
                        .size(11.0)
                        .monospace(),
                );
            }
        });
}

/// Wall-clock time, because "it healed itself twice overnight" is the question
/// the log exists to answer and an elapsed count cannot answer it.
fn stamp(entry: &LogEntry) -> String {
    match entry.at.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            )
        }
        Err(_) => "--:--:--".into(),
    }
}

fn footnotes(ui: &mut egui::Ui, snap: &Snapshot) {
    // Stated as a property of the device, not a warning. On this hardware it is
    // permanently true, and a red banner would make a working app look broken.
    if snap.status == HealthStatus::Degraded {
        ui.label(
            RichText::new(
                "This device publishes no playback info, so PurpToof cannot tell a \
                 pause from a fault. It will not reconnect on silence alone - use \
                 Reconnect if audio has stopped unexpectedly.",
            )
            .size(10.0)
            .color(DIM),
        );
    }

    // This one IS a warning: the number above may not be the phone's audio.
    if snap.scope != MeterScope::Session {
        let detail = match snap.scope {
            MeterScope::Endpoint => {
                "No A2DP session found, so the meter is reading the whole output \
                 device - other apps' audio can show up here."
            }
            MeterScope::SessionGone => {
                "The A2DP session disappeared. Showing silence rather than falling \
                 back to the output device, which would be misleading."
            }
            MeterScope::Session => unreachable!(),
        };
        ui.label(RichText::new(detail).size(10.0).color(AMBER));
    }

    if !snap.device_watch_active || snap.triggers_registered < 2 {
        ui.label(
            RichText::new(format!(
                "Some recovery triggers did not register ({}/3 events, device watch {}). \
                 Recovery still works but may be slower.",
                snap.triggers_registered,
                if snap.device_watch_active {
                    "on"
                } else {
                    "off"
                }
            ))
            .size(10.0)
            .color(AMBER),
        );
    }
}
