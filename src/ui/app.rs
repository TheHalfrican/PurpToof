//! The single view.

use std::time::Duration;

use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};

use crate::core::{Config, HealthStatus, Trigger};
use crate::platform::meter::MeterScope;
use crate::platform::worker::{LogEntry, PEAK_HISTORY};
use crate::platform::{Snapshot, Worker};
use crate::ui::tray::{Tray, TrayAction};

/// Redraw cadence. Matches the supervisor's 10 Hz tick - drawing faster would
/// only re-render identical samples and keep a tray-resident app busy for
/// nothing.
const REPAINT: Duration = Duration::from_millis(100);

// Dark palette. Compact, no decorative chrome, per CLAUDE.md.
//
// The window sits on true black, matching the icon. "Healthy" is deep purple
// rather than the conventional green - it ties the meter to the app's own
// colour, and on black a saturated purple carries as well as green does.

/// The window itself. Pitch black, not egui's default charcoal.
const BG: Color32 = Color32::BLACK;
/// The meter trough. Lifted a hair off black, with a purple cast, or the
/// trough vanishes into the window and the meter loses its frame of reference.
const BG_METER: Color32 = Color32::from_rgb(14, 9, 20);
const GRID: Color32 = Color32::from_rgb(52, 38, 72);
/// Audio is flowing. The app's purple, bright enough to read on black.
const PURPLE: Color32 = Color32::from_rgb(150, 78, 224);
/// The button, and other quiet chrome.
const SURFACE: Color32 = Color32::from_rgb(26, 18, 36);
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
    /// `Err` when the notification area refused us. A missing tray is a
    /// degraded app, not a dead one - but with close-to-tray on it would be a
    /// window the user cannot get back, so that setting is forced off.
    tray: Result<Tray, String>,
    close_to_tray: bool,
    visible: bool,
    /// Whether the initial hide has been applied. eframe has no viewport to
    /// command until the first frame, so start-minimized cannot be honoured
    /// in `new`.
    start_hidden: bool,
}

impl PurpToofApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config) -> Self {
        cc.egui_ctx.set_visuals(visuals());
        let eps = config.silence_eps;
        let tray = Tray::new();
        if let Err(e) = &tray {
            tracing::warn!(error = %e, "tray unavailable; close will exit");
        }
        // Never trap the window behind a tray that does not exist.
        let close_to_tray = config.close_to_tray && tray.is_ok();
        let start_hidden = config.start_minimized && tray.is_ok();
        let worker = Worker::spawn(config).map_err(|e| format!("{e:#}"));
        Self {
            worker,
            log: Vec::new(),
            log_len: 0,
            eps,
            tray,
            close_to_tray,
            visible: !start_hidden,
            start_hidden,
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    /// Tray clicks, and the window's own close button.
    ///
    /// Returns true if the app should keep running.
    fn handle_window_and_tray(&mut self, ctx: &egui::Context) -> bool {
        if self.start_hidden {
            // First frame: eframe now has a viewport to command.
            self.start_hidden = false;
            self.hide(ctx);
        }

        if let Ok(tray) = &self.tray {
            match tray.poll() {
                Some(TrayAction::Quit) => return false,
                Some(TrayAction::Show) => self.show(ctx),
                Some(TrayAction::Toggle) => {
                    if self.visible {
                        self.hide(ctx);
                    } else {
                        self.show(ctx);
                    }
                }
                Some(TrayAction::Reconnect) => {
                    if let Ok(w) = &self.worker {
                        w.trigger(Trigger::Manual);
                    }
                }
                None => {}
            }
        }

        // The close button. With close-to-tray on it hides instead, because
        // this is a background service with a window attached - closing it
        // would stop the thing recovering the audio path, which is the entire
        // point of the app.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide(ctx);
            } else {
                return false;
            }
        }

        true
    }
}

impl eframe::App for PurpToofApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The supervisor ticks on its own thread, so nothing here drives it -
        // this only asks egui to come back and read the next snapshot. It is
        // also what keeps the tray responsive while the window is hidden: with
        // no repaint scheduled, menu clicks would sit unread until something
        // else woke the loop.
        ui.ctx().request_repaint_after(REPAINT);

        let ctx = ui.ctx().clone();
        if !self.handle_window_and_tray(&ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Hidden: the supervisor is still running, there is just nothing to
        // draw. Skipping the body avoids laying out a window nobody sees.
        if !self.visible {
            return;
        }

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

/// Pitch black, with widgets dark enough to sit on it without glowing.
///
/// egui's dark theme is charcoal, which reads as grey next to a true-black
/// icon and title bar.
fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = BG_METER;
    v.faint_bg_color = SURFACE;

    // Buttons and frames. Without this the Reconnect button keeps egui's grey
    // and is the one obviously non-black thing on screen.
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_fill = SURFACE;
        w.weak_bg_fill = SURFACE;
    }
    v.widgets.hovered.bg_fill = Color32::from_rgb(42, 28, 58);
    v.widgets.active.bg_fill = Color32::from_rgb(58, 38, 80);
    v
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
        HealthStatus::Streaming => ("link open", PURPLE),
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
        PURPLE
    } else {
        DIM
    };

    // The level shown by the bar: a short average, not the instantaneous peak.
    //
    // GetPeakValue reports transient peaks, and on real music those hit full
    // scale constantly - the phone was measured at 1.0002, above nominal. A
    // bar driven by that is pinned at maximum whenever anything loud plays and
    // conveys nothing. Averaging one second of samples leaves it well below
    // full scale and actually moving.
    //
    // The history strip below keeps the raw per-sample peaks, so the detail is
    // not lost, only moved to where it reads better.
    let level = average_level(&snap.peak_history, snap.peak);

    // --- current level ------------------------------------------------------
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, BG_METER);

    // Linear, deliberately.
    //
    // This was square-rooted, on the theory that peaks live in the low end of
    // 0..1 and a linear bar would sit mostly empty. In practice the observed
    // range on real music is about 0.2 to 0.75, and sqrt maps that to 0.47 to
    // 0.85 - a narrow band pinned against the right edge, so the meter reads
    // as permanently maxed and stops distinguishing loud from very loud.
    //
    // Linear spends the full width on the range the audio actually occupies,
    // and leaves visible headroom above it.
    let filled = level.clamp(0.0, 1.0);
    if filled > 0.0 {
        let mut bar = rect;
        bar.set_width(rect.width() * filled);
        painter.rect_filled(bar, 2.0, level_color);
    }
    painter.text(
        rect.right_center() - egui::vec2(6.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{level:.4}"),
        egui::FontId::monospace(11.0),
        Color32::from_rgb(200, 200, 205),
    );

    // --- history ------------------------------------------------------------
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, BG_METER);

    // The silence threshold, drawn so the user can see what the watchdog sees
    // rather than guessing where "silent" begins.
    // Clamped to a visible minimum: at the default eps of 0.0005 a linear
    // position would put this line half a pixel off the floor, where it reads
    // as part of the border rather than as a threshold.
    let eps_y = rect.bottom() - rect.height() * eps.max(0.012);
    painter.line_segment(
        [
            egui::pos2(rect.left(), eps_y),
            egui::pos2(rect.right(), eps_y),
        ],
        Stroke::new(1.0, GRID),
    );

    let slot = rect.width() / PEAK_HISTORY as f32;
    for (i, sample) in snap.peak_history.iter().enumerate() {
        // Linear, to match the bar above. Two different curves on the same
        // screen would disagree about how loud the same moment was.
        let h = sample.clamp(0.0, 1.0) * rect.height();
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

/// Mean of the last second of peak samples, at the 10 Hz tick rate.
///
/// Falls back to the instantaneous value before any history exists, so the
/// meter is not blank for the first tick after launch.
fn average_level(history: &std::collections::VecDeque<f32>, fallback: f32) -> f32 {
    const WINDOW: usize = 10;
    if history.is_empty() {
        return fallback;
    }
    let n = history.len().min(WINDOW);
    history.iter().rev().take(n).sum::<f32>() / n as f32
}

fn status_line(ui: &mut egui::Ui, snap: &Snapshot) {
    let (text, color) = match snap.status {
        HealthStatus::Streaming => ("Streaming".to_string(), PURPLE),
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

#[cfg(test)]
mod tests {
    use super::average_level;
    use std::collections::VecDeque;

    #[test]
    fn an_empty_history_falls_back_to_the_instant_value() {
        // Otherwise the meter is blank for the first tick after launch.
        assert_eq!(average_level(&VecDeque::new(), 0.42), 0.42);
    }

    #[test]
    fn transient_peaks_do_not_peg_the_bar() {
        // The actual complaint: music that touches full scale on transients
        // pinned the bar at maximum. One full-scale sample among nine quiet
        // ones must not.
        let mut h: VecDeque<f32> = VecDeque::from(vec![0.2; 9]);
        h.push_back(1.0);
        let level = average_level(&h, 0.0);
        assert!(level < 0.35, "one transient still dominated: {level}");
    }

    #[test]
    fn sustained_loudness_still_reads_loud() {
        // The averaging must not flatten everything - genuinely loud audio
        // should still fill most of the bar.
        let h: VecDeque<f32> = VecDeque::from(vec![0.9; 10]);
        assert!(average_level(&h, 0.0) > 0.85);
    }

    #[test]
    fn only_the_last_second_counts() {
        // Older samples must age out, or the bar lags far behind the audio.
        let mut h: VecDeque<f32> = VecDeque::from(vec![1.0; 100]);
        for _ in 0..10 {
            h.push_back(0.0);
        }
        assert_eq!(average_level(&h, 0.0), 0.0, "stale samples still counted");
    }
}
