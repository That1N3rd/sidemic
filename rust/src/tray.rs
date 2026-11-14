//! System-tray front end. All the real work happens on background threads in Client / Host.

use crate::{log, win};
use std::path::Path;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

pub struct TrayOptions<'a> {
    pub title: &'a str,
    pub status: Box<dyn Fn() -> String + 'a>,
    pub mute: Option<(Box<dyn Fn(bool) + 'a>, Box<dyn Fn() -> bool + 'a>)>,
    pub pairing_key: Option<String>,
    pub config_path: &'a Path,
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).collect::<String>() + "…" }
}

fn load_icon() -> Icon {
    if let Ok(i) = Icon::from_resource(1, Some((32, 32))) {
        return i;
    }
    // Fallback: a plain blue disc.
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let inside = dx * dx + dy * dy <= 15.0 * 15.0;
            rgba.extend_from_slice(if inside { &[30, 136, 229, 255] } else { &[0, 0, 0, 0] });
        }
    }
    Icon::from_rgba(rgba, w, h).expect("fallback icon")
}

/// Runs the tray UI on the calling thread until the user picks Exit.
pub fn run(opts: TrayOptions) {
    let menu = Menu::new();
    let status_item = MenuItem::new(opts.title, false, None);
    let _ = menu.append(&status_item);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let mute_item = opts.mute.as_ref().map(|(_, is_muted)| CheckMenuItem::new("Mute microphone", true, is_muted(), None));
    if let Some(m) = &mute_item {
        let _ = menu.append(m);
    }
    let key_item = opts.pairing_key.as_ref().map(|_| MenuItem::new("Copy pairing key", true, None));
    if let Some(k) = &key_item {
        let _ = menu.append(k);
    }
    let config_item = MenuItem::new("Open config file", true, None);
    let log_item = MenuItem::new("Open log", true, None);
    let exit_item = MenuItem::new("Exit", true, None);
    let _ = menu.append(&config_item);
    let _ = menu.append(&log_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit_item);

    let tooltip = |s: &str| shorten(&format!("MicBridge - {s}"), 120);
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip(&(opts.status)()))
        .with_icon(load_icon())
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            win::message_box(&format!("Could not create tray icon: {e}"));
            return;
        }
    };

    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();
    let mut last_tick = Instant::now() - Duration::from_secs(2);

    loop {
        win::pump_messages();

        while let Ok(ev) = menu_rx.try_recv() {
            if ev.id() == exit_item.id() {
                drop(tray);
                return;
            } else if ev.id() == config_item.id() {
                win::open_path(&opts.config_path.display().to_string());
            } else if ev.id() == log_item.id() {
                match log::file_path() {
                    Some(p) => win::open_path(&p.display().to_string()),
                    None => win::message_box(&log::recent_text()),
                }
            } else if key_item.as_ref().map(|k| ev.id() == k.id()).unwrap_or(false) {
                if let Some(key) = &opts.pairing_key {
                    if win::set_clipboard(key) {
                        win::message_box("Pairing key copied to the clipboard.\n\nOn the laptop run:\n  micbridge client --host <this-pc-ip> --key <paste>");
                    } else {
                        win::message_box(&format!("Could not copy to the clipboard. Pairing key:\n\n{key}"));
                    }
                }
            } else if let Some(m) = &mute_item {
                if ev.id() == m.id() {
                    if let Some((set, _)) = &opts.mute {
                        set(m.is_checked());
                    }
                }
            }
        }

        while let Ok(ev) = tray_rx.try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = ev {
                win::message_box(&(opts.status)());
            }
        }

        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            let s = (opts.status)();
            let _ = tray.set_tooltip(Some(tooltip(&s)));
            status_item.set_text(shorten(&s, 100));
            if let (Some(m), Some((_, is_muted))) = (&mute_item, &opts.mute) {
                if m.is_checked() != is_muted() {
                    m.set_checked(is_muted());
                }
            }
        }

        std::thread::sleep(Duration::from_millis(30));
    }
}
