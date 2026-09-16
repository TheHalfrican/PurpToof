//! The single view.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};

use crate::core::power::RadioPowerPolicy;
use crate::core::{Config, HealthStatus, Paths, Trigger};
use crate::platform::autostart;
use crate::platform::meter::MeterScope;
use crate::platform::radio_power;
use crate::platform::worker::{LogEntry, PEAK_HISTORY};
use crate::platform::{Snapshot, Worker};
use crate::ui::tray::{Tray, TrayAction};

/// Redraw cadence while the window is hidden in the tray.
///
/// Matches the supervisor's 10 Hz tick. Nothing is on screen, so this exists
/// only to keep the tray menu being pumped - see `logic()`.
const REPAINT_HIDDEN: Duration = Duration::from_millis(100);

/// Redraw cadence while the window is on screen.
///
/// Faster than the 10 Hz sample rate on purpose. The samples still arrive at
/// 10 Hz, but the bar is eased toward them between frames, so motion is
/// continuous instead of stepping ten times a second. CLAUDE.md calls the
/// meter "the feature"; a jerky one undersells a working link. Costs nothing
/// while hidden, which is most of the time.
const REPAINT_VISIBLE: Duration = Duration::from_millis(16);

/// How fast the drawn bar chases the ballistic level, as a time constant.
///
/// Small enough to be imperceptible next to the 200ms attack; its only job is
/// to turn 10 Hz steps into continuous motion.
const DISPLAY_TAU: f32 = 0.04;

/// Meter ballistics, per 100ms sample: rise fast, fall slowly.
///
/// A flat mean over one second used to drive this bar. It was added because
/// transients pinned the bar at full scale, and it fixed that - but a mean is
/// symmetric, so it also lagged half a second behind on the way *up*, which
/// read as sluggish.
///
/// Asymmetric ballistics is what hardware meters do, and it solves both. The
/// original complaint was never "it rises too fast", it was "one transient
/// pins it and it stays pinned"; a fast attack with a slow release makes a
/// transient a bump that falls away, which is what a meter is supposed to
/// look like.
///
/// `ATTACK` reaches ~90% of a step in 200ms, `RELEASE` decays to ~10% in
/// 600ms.
const ATTACK: f32 = 0.68;
const RELEASE: f32 = 0.32;

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
    /// Set when the user chose Quit from the tray.
    ///
    /// Without it, close-to-tray cancels our own shutdown: Quit asks the
    /// viewport to close, the next pass sees `close_requested` and cannot tell
    /// that request apart from the user pressing X, so it cancels and hides.
    /// Quit then silently does nothing.
    quitting: bool,
    config: Config,
    paths: Paths,
    /// Surfaced next to the settings when a save fails - a silently ignored
    /// checkbox is worse than one that says why it did not stick.
    settings_error: Option<String>,
    /// The adapter power policy as last re-read here, overriding the worker's
    /// startup reading once a fix has been applied. `None` until then.
    radio_power_now: Option<RadioPowerPolicy>,
    /// The level the bar is currently drawn at, eased toward the ballistic
    /// level each frame. Separate from the ballistics so that smoothing the
    /// *animation* cannot change what the meter actually reports.
    display_level: f32,
    /// Timestamp of the previous frame, so the easing is frame-rate
    /// independent rather than assuming a fixed step.
    last_frame: Option<Instant>,
    /// When to re-read that policy next.
    ///
    /// Set only while a fix is in flight. `ShellExecuteExW` returns when the
    /// UAC prompt is answered, not when the change has landed, so the banner
    /// watches for the value to flip rather than assuming it worked.
    radio_recheck_at: Option<Instant>,
    /// Whether the initial hide has been applied. eframe has no viewport to
    /// command until the first frame, so start-minimized cannot be honoured
    /// in `new`.
    start_hidden: bool,
}

impl PurpToofApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, paths: Paths) -> Self {
        cc.egui_ctx.set_visuals(visuals());
        let eps = config.silence_eps;
        let tray = Tray::new();
        if let Err(e) = &tray {
            tracing::warn!(error = %e, "tray unavailable; close will exit");
        }
        // Never trap the window behind a tray that does not exist.
        let close_to_tray = config.close_to_tray && tray.is_ok();
        let start_hidden = config.start_minimized && tray.is_ok();
        let settings = config.clone();
        let worker = Worker::spawn(config).map_err(|e| format!("{e:#}"));
        Self {
            config: settings,
            paths,
            settings_error: None,
            display_level: 0.0,
            last_frame: None,
            radio_power_now: None,
            radio_recheck_at: None,
            worker,
            log: Vec::new(),
            log_len: 0,
            eps,
            tray,
            close_to_tray,
            quitting: false,
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
                Some(TrayAction::Quit) => {
                    self.quitting = true;
                    return false;
                }
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
            if self.close_to_tray && !self.quitting {
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
    /// Tray and window handling, every pass.
    ///
    /// This MUST live here rather than in `ui`. eframe runs no egui pass at all
    /// while the window is hidden, so `ui` is never called then - and putting
    /// the tray polling there meant that the moment you closed to tray, the
    /// entire tray menu went dead: Quit, Show and Reconnect all stopped
    /// responding, which is precisely when a tray app needs them most.
    ///
    /// The repaint request is what keeps this being called at all while
    /// hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(if self.visible {
            REPAINT_VISIBLE
        } else {
            REPAINT_HIDDEN
        });

        if !self.handle_window_and_tray(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Belt and braces: eframe should not call this while hidden, but
        // laying out a window nobody can see would be wasted work.
        if !self.visible {
            return;
        }

        if let Err(e) = &self.worker {
            let message = e.clone();
            egui::CentralPanel::default().show(ui, |ui| startup_error(ui, &message));
            return;
        }

        // Take the snapshot and refresh the log BEFORE the closure. Holding a
        // borrow of `self.worker` across it would conflict with the settings
        // panel, which needs `&mut self`.
        let Some(snap) = self.worker.as_ref().map(|w| w.snapshot()).ok() else {
            return;
        };
        if snap.log_len != self.log_len {
            if let Ok(w) = &self.worker {
                self.log = w.log();
            }
            self.log_len = snap.log_len;
        }

        // Collected rather than acted on inline, for the same reason.
        let mut reconnect = false;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;

            device_row(ui, &snap);
            ui.add_space(2.0);
            let target = ballistic_level(&snap.peak_history, snap.peak);
            self.display_level = ease(self.display_level, target, &mut self.last_frame);
            meter(ui, &snap, self.eps, self.display_level);
            status_line(ui, &snap);

            if ui
                .add_sized(
                    [ui.available_width(), 30.0],
                    egui::Button::new(RichText::new("Reconnect").size(14.0)),
                )
                .on_hover_text(
                    "Tears the connection down and rebuilds it. Safe at any time -                      and the intended fix when audio has died but the link still                      claims to be open.",
                )
                .clicked()
            {
                reconnect = true;
            }

            ui.separator();
            settings(ui, self);
            ui.separator();

            // Footnotes first, laid out bottom-up, so the log can then expand
            // into whatever height is left rather than leaving dead space.
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                footnotes(ui, &snap, self);
                ui.separator();
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    reconnect_log(ui, &self.log);
                });
            });
        });

        if reconnect && let Ok(w) = &self.worker {
            w.trigger(Trigger::Manual);
        }
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
fn meter(ui: &mut egui::Ui, snap: &Snapshot, eps: f32, level: f32) {
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
    // `level` arrives already shaped: ballistics applied, then eased toward
    // for this frame. GetPeakValue reports transient peaks and on real music
    // those hit full scale constantly - the phone was measured at 1.0002,
    // above nominal - so the raw value cannot drive a bar directly.
    //
    // The history strip below keeps the raw per-sample peaks, so the detail is
    // not lost, only moved to where it reads better.

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

/// Peak history run through fast-attack / slow-release ballistics.
///
/// Stateless: it replays the whole retained history each call rather than
/// keeping a running level. At 150 samples that is free, and it keeps this a
/// pure function of the snapshot - so it stays testable, and two frames drawn
/// from the same snapshot cannot disagree.
///
/// Falls back to the instantaneous value before any history exists, so the
/// meter is not blank for the first tick after launch.
fn ballistic_level(history: &std::collections::VecDeque<f32>, fallback: f32) -> f32 {
    if history.is_empty() {
        return fallback;
    }
    let mut level = 0.0;
    for &sample in history {
        let coeff = if sample > level { ATTACK } else { RELEASE };
        level += (sample - level) * coeff;
    }
    level
}

/// Ease the drawn level toward the ballistic one, frame-rate independently.
///
/// Uses real elapsed time rather than a fixed step per frame, so the bar moves
/// at the same speed whether the compositor is giving us 60fps or 30.
fn ease(current: f32, target: f32, last_frame: &mut Option<Instant>) -> f32 {
    let now = Instant::now();
    let dt = last_frame
        .replace(now)
        .map(|prev| now.saturating_duration_since(prev).as_secs_f32())
        // A long gap - the window was hidden, or the machine slept. Snapping
        // is right: easing across it would animate a value nobody was
        // watching.
        .filter(|dt| *dt < 0.5)
        .unwrap_or(1.0);

    current + (target - current) * (1.0 - (-dt / DISPLAY_TAU).exp())
}

/// The settings panel. Collapsed by default - the meter is what people open
/// the window for, and settings are changed once and then forgotten.
fn settings(ui: &mut egui::Ui, app: &mut PurpToofApp) {
    egui::CollapsingHeader::new(RichText::new("Settings").size(12.0).color(DIM))
        .default_open(false)
        .show(ui, |ui| {
            let mut changed = false;

            // Autostart writes to the registry as well as the config, and the
            // registry is the thing that actually has the effect - so it is
            // applied first and the config only records what was asked for.
            let mut autostart_on = app.config.autostart;
            if ui
                .checkbox(&mut autostart_on, "Start with Windows")
                .on_hover_text(
                    "Adds a per-user Run entry. No administrator prompt, and it                      starts only for your account.",
                )
                .changed()
            {
                match autostart::set(autostart_on) {
                    Ok(()) => {
                        app.config.autostart = autostart_on;
                        changed = true;
                    }
                    Err(e) => app.settings_error = Some(e),
                }
            }

            if ui
                .checkbox(&mut app.config.start_minimized, "Start hidden in the tray")
                .changed()
            {
                changed = true;
            }

            let tray_ok = app.tray.is_ok();
            ui.add_enabled_ui(tray_ok, |ui| {
                if ui
                    .checkbox(&mut app.config.close_to_tray, "Close button hides to tray")
                    .on_hover_text(if tray_ok {
                        "Off means the close button exits PurpToof and stops it                          watching the audio path."
                    } else {
                        "Unavailable: the notification area could not be reached,                          so there would be no way to get the window back."
                    })
                    .changed()
                {
                    app.close_to_tray = app.config.close_to_tray;
                    changed = true;
                }
            });

            ui.horizontal(|ui| {
                let mut secs = app.config.silence_timeout_ms as f32 / 1000.0;
                if ui
                    .add(
                        egui::Slider::new(&mut secs, 1.0..=30.0)
                            .suffix(" s")
                            .text(RichText::new("Silence before recovery").size(11.0)),
                    )
                    .on_hover_text(
                        "How long the remote may claim to be playing while the meter                          reads silence before PurpToof rebuilds the connection.",
                    )
                    .changed()
                {
                    app.config.silence_timeout_ms = (secs * 1000.0) as u64;
                    changed = true;
                }
            });

            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Settings: {}", app.paths.config.display()))
                    .size(9.0)
                    .color(DIM),
            );
            ui.label(
                RichText::new(format!("Logs: {}", app.paths.log_dir.display()))
                    .size(9.0)
                    .color(DIM),
            );

            if let Some(e) = &app.settings_error {
                ui.label(RichText::new(e).size(10.0).color(AMBER));
            }

            if changed {
                // Written immediately rather than on exit: a tray app is often
                // killed rather than closed, and settings that quietly did not
                // persist are worse than settings that cannot be changed.
                match app.config.save(&app.paths.config) {
                    Ok(()) => app.settings_error = None,
                    Err(e) => app.settings_error = Some(e),
                }
                // Timeouts are read by the supervisor at construction, so a
                // change to them needs a restart to take effect. Say so rather
                // than let it look broken.
                tracing::info!("settings saved");
            }
        });
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

fn footnotes(ui: &mut egui::Ui, snap: &Snapshot, app: &mut PurpToofApp) {
    radio_power_banner(ui, snap, app);

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

/// How often to re-read the adapter policy while a fix is in flight.
const RADIO_RECHECK: Duration = Duration::from_secs(1);

/// The adapter power-management banner, and the one-click fix.
///
/// Shown only when the policy was **positively** read as "may power down".
/// `Unknown` stays silent - see `core::power` on why an absent value is not
/// evidence of anything.
///
/// The exact change is spelled out *before* the button rather than after.
/// "One click" must not mean "one click and you find out afterwards what
/// happened", and the UAC prompt names PowerShell, which explains nothing on
/// its own.
fn radio_power_banner(ui: &mut egui::Ui, snap: &Snapshot, app: &mut PurpToofApp) {
    // A fix is in flight: re-read on a timer until the value flips. The
    // elevated helper runs *after* `ShellExecuteExW` returns, so there is
    // nothing to read at the moment of the click and the banner has to wait
    // for the change rather than congratulate itself.
    if let (Some(at), Some(devnode)) = (app.radio_recheck_at, snap.adapter_devnode.as_deref())
        && Instant::now() >= at
    {
        let latest = radio_power::policy_for(devnode);
        app.radio_power_now = Some(latest);
        app.radio_recheck_at = latest.should_warn().then(|| Instant::now() + RADIO_RECHECK);
        ui.ctx().request_repaint_after(RADIO_RECHECK);
    }

    let policy = app.radio_power_now.unwrap_or(snap.radio_power);
    if !policy.should_warn() {
        return;
    }
    // Nothing to act on without the devnode, and a warning the user cannot do
    // anything about is just noise.
    let Some(devnode) = snap.adapter_devnode.clone() else {
        return;
    };

    ui.label(
        RichText::new("Windows is allowed to power down the Bluetooth adapter.")
            .size(10.0)
            .color(AMBER),
    );
    ui.label(
        RichText::new(radio_power::FIX_DESCRIPTION)
            .size(10.0)
            .color(DIM),
    );

    if app.radio_recheck_at.is_some() {
        ui.label(
            RichText::new("Waiting for Windows to apply it...")
                .size(10.0)
                .color(DIM),
        );
        return;
    }

    if ui.button("Stop Windows powering it down").clicked() {
        match radio_power::request_hold_radio_on(&devnode) {
            Ok(()) => app.radio_recheck_at = Some(Instant::now() + RADIO_RECHECK),
            // Declining the prompt lands here too. That is an ordinary answer,
            // so it is reported where the other settings failures are and not
            // as a fault.
            Err(e) => app.settings_error = Some(format!("could not start the fix: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ballistic_level;
    use std::collections::VecDeque;

    #[test]
    fn an_empty_history_falls_back_to_the_instant_value() {
        // Otherwise the meter is blank for the first tick after launch.
        assert_eq!(ballistic_level(&VecDeque::new(), 0.42), 0.42);
    }

    #[test]
    fn transient_peaks_are_a_bump_that_falls_away_not_a_peg() {
        // The original complaint was "music that touches full scale on
        // transients pinned the bar at maximum". A one-second mean fixed it by
        // hiding transients entirely, which is what made the bar feel
        // sluggish. The fix now is decay, not suppression: a transient is
        // allowed to show, and must then fall back on its own.
        let mut h: VecDeque<f32> = VecDeque::from(vec![0.2; 9]);
        h.push_back(1.0);
        let spike = ballistic_level(&h, 0.0);
        assert!(spike > 0.5, "the transient should be visible: {spike}");
        assert!(spike < 0.85, "but must not reach full scale: {spike}");

        // 600ms of quiet later it is back down near the floor - which is the
        // half the old averaging got right and must not be lost.
        for _ in 0..6 {
            h.push_back(0.2);
        }
        let settled = ballistic_level(&h, 0.0);
        assert!(settled < 0.3, "the bump did not fall away: {settled}");
    }

    #[test]
    fn sustained_loudness_still_reads_loud() {
        // Ballistics must not flatten anything - genuinely loud audio should
        // still fill most of the bar.
        let h: VecDeque<f32> = VecDeque::from(vec![0.9; 10]);
        assert!(ballistic_level(&h, 0.0) > 0.85);
    }

    #[test]
    fn the_attack_reaches_most_of_a_step_within_200ms() {
        // The sluggishness this replaced: a one-second mean needed ten samples
        // to get here. Two is the whole point.
        let h: VecDeque<f32> = VecDeque::from(vec![1.0; 2]);
        let level = ballistic_level(&h, 0.0);
        assert!(level > 0.85, "attack too slow: {level} after 200ms");
    }

    #[test]
    fn stale_samples_decay_away() {
        // Replaces a test that asserted the level hit exactly 0.0 once the
        // averaging window had rolled past. Release is exponential, so it
        // approaches zero rather than reaching it - the assertion has to be a
        // bound, not an equality.
        let mut h: VecDeque<f32> = VecDeque::from(vec![1.0; 100]);
        for _ in 0..6 {
            h.push_back(0.0);
        }
        let level = ballistic_level(&h, 0.0);
        assert!(
            level < 0.15,
            "600ms of silence should be near zero: {level}"
        );
    }

    #[test]
    fn the_bar_rises_faster_than_it_falls() {
        // The asymmetry itself, stated directly: a symmetric filter is what
        // made the meter feel slow, so a regression to one should fail here
        // rather than only being noticed by eye.
        let up: VecDeque<f32> = VecDeque::from(vec![1.0; 3]);
        let rise = ballistic_level(&up, 0.0);

        let mut down: VecDeque<f32> = VecDeque::from(vec![1.0; 50]);
        for _ in 0..3 {
            down.push_back(0.0);
        }
        let fall = ballistic_level(&down, 0.0);

        assert!(
            rise > 1.0 - fall,
            "attack ({rise}) should cover more ground than release ({fall}) does"
        );
    }
}
