//! System-tray icon. Lives on the main thread. While the settings window is open, eframe's event loop pumps
//! its messages and `App::update` calls `Tray::poll`; while the window is closed, `Tray::idle_loop` pumps.

use crate::state::Shared;
use crate::{log, win};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).collect::<String>() + "…" }
}

/// RGBA microphone glyph on a blue disc, drawn in code so no image decoder is needed.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (cx, cy) = (s / 2.0, s / 2.0);
            let in_disc = (px - cx).powi(2) + (py - cy).powi(2) <= (s / 2.0 - 0.5).powi(2);
            let (cw, top, bottom) = (s * 0.28, s * 0.18, s * 0.60);
            let r = cw / 2.0;
            let in_capsule = (px - cx).abs() <= r
                && ((py >= top + r && py <= bottom - r)
                    || (px - cx).powi(2) + (py - (top + r)).powi(2) <= r * r
                    || (px - cx).powi(2) + (py - (bottom - r)).powi(2) <= r * r);
            let arc_r = s * 0.23;
            let arc_cy = s * 0.53;
            let d = ((px - cx).powi(2) + (py - arc_cy).powi(2)).sqrt();
            let in_arc = py >= arc_cy && (d - arc_r).abs() <= s * 0.045;
            let in_stem = (px - cx).abs() <= s * 0.04 && py >= arc_cy + arc_r && py <= s * 0.86;
            let in_base = (px - cx).abs() <= s * 0.14 && (py - s * 0.86).abs() <= s * 0.04;
            let white = in_capsule || in_arc || in_stem || in_base;
            rgba.extend_from_slice(if white && in_disc {
                &[255, 255, 255, 255]
            } else if in_disc {
                &[30, 136, 229, 255]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    rgba
}

fn load_icon() -> Icon {
    if let Ok(i) = Icon::from_resource(1, Some((32, 32))) {
        return i;
    }
    Icon::from_rgba(icon_rgba(32), 32, 32).expect("fallback icon")
}

pub struct Tray {
    shared: Arc<Shared>,
    _tray: TrayIcon,
    tray_icon_ok: bool,
    status_item: MenuItem,
    open_item: MenuItem,
    mute_item: CheckMenuItem,
    key_item: MenuItem,
    log_item: MenuItem,
    exit_item: MenuItem,
    last_tick: Instant,
}

impl Tray {
    pub fn new(shared: Arc<Shared>) -> Option<Tray> {
        let menu = Menu::new();
        let status_item = MenuItem::new("SideMic", false, None);
        let open_item = MenuItem::new("Open SideMic", true, None);
        let mute_item = CheckMenuItem::new("Mute microphone", false, false, None);
        let key_item = MenuItem::new("Copy pairing key", false, None);
        let log_item = MenuItem::new("Open log", true, None);
        let exit_item = MenuItem::new("Exit", true, None);
        let _ = menu.append(&status_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&open_item);
        let _ = menu.append(&mute_item);
        let _ = menu.append(&key_item);
        let _ = menu.append(&log_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&exit_item);

        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("SideMic - {}", shared.status()))
            .with_icon(load_icon())
            .build()
        {
            Ok(t) => t,
            Err(e) => {
                log::error(format!("Could not create tray icon: {e}"));
                return None;
            }
        };
        Some(Tray {
            shared,
            _tray: tray,
            tray_icon_ok: true,
            status_item,
            open_item,
            mute_item,
            key_item,
            log_item,
            exit_item,
            last_tick: Instant::now() - Duration::from_secs(2),
        })
    }

    /// Handles pending menu/tray events and refreshes the tooltip once a second. Call often.
    pub fn poll(&mut self) {
        let shared = &self.shared;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id() == self.exit_item.id() {
                shared.request_exit();
            } else if ev.id() == self.open_item.id() {
                shared.request_show();
            } else if ev.id() == self.log_item.id() {
                match log::file_path() {
                    Some(p) => win::open_path(&p.display().to_string()),
                    None => win::message_box(&log::recent_text()),
                }
            } else if ev.id() == self.key_item.id() {
                let key = shared.pairing_key();
                if !key.is_empty() {
                    if win::set_clipboard(&key) {
                        win::message_box("Pairing key copied to the clipboard.\n\nPaste it into SideMic on the laptop.");
                    } else {
                        win::message_box(&format!("Could not copy to the clipboard. Pairing key:\n\n{key}"));
                    }
                }
            } else if ev.id() == self.mute_item.id() {
                shared.set_muted(self.mute_item.is_checked());
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = ev {
                shared.request_show();
            }
        }
        if self.tray_icon_ok && self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.last_tick = Instant::now();
            let s = shared.status();
            let _ = self._tray.set_tooltip(Some(shorten(&format!("SideMic - {s}"), 120)));
            self.status_item.set_text(shorten(&s, 100));
            let client = shared.client_running();
            self.mute_item.set_enabled(client);
            if client && self.mute_item.is_checked() != shared.muted() {
                self.mute_item.set_checked(shared.muted());
            }
            self.key_item.set_enabled(!shared.pairing_key().is_empty());
        }
    }

    /// Pumps messages while the window is closed. Returns when the window should open or the app should exit.
    pub fn idle_loop(&mut self) {
        loop {
            win::pump_messages();
            self.poll();
            if self.shared.exit.load(Ordering::SeqCst) || self.shared.show_window.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
