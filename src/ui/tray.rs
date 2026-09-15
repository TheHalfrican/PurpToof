//! The tray icon and its menu.
//!
//! # Why this lives on the UI thread
//!
//! `tray-icon` needs a Win32 message pump, and eframe's winit loop is the only
//! one this process has. Creating the icon anywhere else - the supervisor
//! thread in particular, which is MTA and pumpless - gets an icon that never
//! receives a click.
//!
//! Events arrive on global channels rather than callbacks, so the app polls
//! them once per frame. At the 10 Hz repaint rate a menu click is acted on
//! within 100ms, which is imperceptible for "show the window".

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// What the user asked of the tray this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Bring the window back and focus it.
    Show,
    /// Toggle between shown and hidden.
    Toggle,
    /// Force a reconnect without opening the window.
    Reconnect,
    /// Actually exit - the only route out when close-to-tray is on.
    Quit,
}

pub struct Tray {
    /// Held for its lifetime: dropping it removes the icon from the tray.
    _icon: TrayIcon,
    show: MenuId,
    reconnect: MenuId,
    quit: MenuId,
}

impl Tray {
    /// Build the tray icon.
    ///
    /// Returns `Err` with a readable reason rather than panicking; a missing
    /// tray is a degraded app, not a dead one, and on a machine where the
    /// shell notification area is unavailable the window still works.
    pub fn new() -> Result<Self, String> {
        let icon = tray_icon_image()?;

        let show = MenuItem::new("Show PurpToof", true, None);
        let reconnect = MenuItem::new("Reconnect", true, None);
        let quit = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&show).map_err(|e| e.to_string())?;
        menu.append(&reconnect).map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;

        let ids = (show.id().clone(), reconnect.id().clone(), quit.id().clone());

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("PurpToof")
            .with_icon(icon)
            .build()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            _icon: tray,
            show: ids.0,
            reconnect: ids.1,
            quit: ids.2,
        })
    }

    /// Drain this frame's tray and menu events.
    ///
    /// Returns at most one action; if several arrived, the most decisive wins
    /// so a stray double-click cannot undo an explicit Quit.
    pub fn poll(&self) -> Option<TrayAction> {
        let mut action = None;

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let next = if event.id == self.quit {
                Some(TrayAction::Quit)
            } else if event.id == self.reconnect {
                Some(TrayAction::Reconnect)
            } else if event.id == self.show {
                Some(TrayAction::Show)
            } else {
                None
            };
            action = merge(action, next);
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            // Left click toggles, which is what every tray app does and what
            // people try first. Right click opens the menu and is handled for
            // us.
            if let TrayIconEvent::DoubleClick { .. } = event {
                action = merge(action, Some(TrayAction::Toggle));
            }
        }

        action
    }
}

/// Quit outranks everything; otherwise the later event wins.
fn merge(current: Option<TrayAction>, next: Option<TrayAction>) -> Option<TrayAction> {
    match (current, next) {
        (Some(TrayAction::Quit), _) => Some(TrayAction::Quit),
        (_, Some(n)) => Some(n),
        (c, None) => c,
    }
}

/// The same artwork the window uses, handed to the tray.
fn tray_icon_image() -> Result<Icon, String> {
    let data = super::icon_data();
    Icon::from_rgba(data.rgba, data.width, data.height).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_cannot_be_overridden_by_a_later_event() {
        // A click that lands in the same frame as Quit must not resurrect the
        // window instead of exiting.
        assert_eq!(
            merge(Some(TrayAction::Quit), Some(TrayAction::Toggle)),
            Some(TrayAction::Quit)
        );
        assert_eq!(
            merge(Some(TrayAction::Quit), Some(TrayAction::Show)),
            Some(TrayAction::Quit)
        );
    }

    #[test]
    fn quit_wins_regardless_of_arrival_order() {
        assert_eq!(
            merge(Some(TrayAction::Show), Some(TrayAction::Quit)),
            Some(TrayAction::Quit)
        );
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert_eq!(merge(None, None), None);
        assert_eq!(merge(Some(TrayAction::Show), None), Some(TrayAction::Show));
    }
}
