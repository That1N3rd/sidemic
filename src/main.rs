//! SideMic: send your laptop's microphone to the PC you stream from (Sunshine/Apollo + Moonlight).
//! A sidecar, not a fork, so Moonlight/Sunshine updates can't break it.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod client;
mod config;
mod gui;
mod host;
mod log;
mod protocol;
mod state;
mod tray;
mod win;

use config::Config;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

const USAGE: &str = "\
SideMic - send your laptop's microphone to the PC you stream from (Sunshine/Apollo + Moonlight).
Independent of Moonlight and Sunshine, so their updates never break it.

Just run sidemic.exe to open the settings window. Command-line use is optional:

On the streamed PC (host):      sidemic host [--port 48020] [--output \"CABLE Input\"]
On the laptop (client):         sidemic client --host <host-ip> --key <key-from-host> [--mic \"name\"]
List audio devices:             sidemic devices
Allow UDP port in firewall:     sidemic firewall        (host only, asks for admin)
Start with Windows:             sidemic autostart       (sidemic autostart --remove to undo)

Options:
  --minimized       start in the tray without opening the window
  --console         run in the terminal instead of the tray (host/client verbs)
  --port N          UDP port (default 48020, same on both ends)
  --key K           shared key; host generates one on first run
  --new-key         host: generate a fresh key
  --mic NAME        client: part of the microphone's name (default: Windows default mic)
  --output NAME     host: part of the playback device name that feeds the virtual mic
  --bitrate N       Opus bits/s (default 40000)
  --jitter MS       host: jitter cushion (default 60)
  --follow / --no-follow   client: only send while Moonlight.exe runs (default: follow)
  --config PATH     use a different sidemic.json

Settings are saved to sidemic.json next to the exe.
";

struct Args {
    verb: String,
    opts: HashMap<String, String>,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut verb = String::new();
    let mut opts = HashMap::new();
    let takes_value = ["host", "port", "key", "mic", "output", "bitrate", "jitter", "config"];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') || a.starts_with('/') {
            let mut name = a.trim_start_matches(['-', '/']).to_lowercase();
            let mut val = String::new();
            if let Some(eq) = name.find('=') {
                val = name[eq + 1..].to_string();
                name.truncate(eq);
            } else if takes_value.contains(&name.as_str()) && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                i += 1;
                val = args[i].clone();
            }
            opts.insert(name, val);
        } else if verb.is_empty() {
            verb = a.to_lowercase();
        }
        i += 1;
    }
    Args { verb, opts }
}

fn show(text: &str, console: bool) {
    if console {
        println!("{text}");
    } else {
        win::message_box(text);
    }
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let Args { verb, opts } = parse_args();
    let console = opts.contains_key("console") || verb == "devices" || verb == "help";
    let console = if console { win::enable_console() } else { false };

    if verb == "help" || opts.contains_key("help") || opts.contains_key("h") || opts.contains_key("?") {
        show(USAGE, console);
        return 0;
    }

    let cfg_path: PathBuf = opts.get("config").map(PathBuf::from).unwrap_or_else(config::default_path);
    let log_dir = cfg_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(config::data_dir);
    log::open(&log_dir.join("sidemic.log"));
    log::set_console(console);

    if verb == "devices" {
        show(&audio::describe(), console);
        return 0;
    }

    let mut cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            show(&format!("Could not read {}:\n{e}", cfg_path.display()), console);
            return 2;
        }
    };
    // Save under the new name even if we loaded a legacy micbridge.json.
    cfg.path = log_dir.join("sidemic.json");
    if let Some(p) = opts.get("config") {
        cfg.path = PathBuf::from(p);
    }

    if verb == "host" || verb == "client" {
        cfg.mode = verb.clone();
    }
    if let Some(h) = opts.get("host") {
        if verb == "client" {
            cfg.host = h.clone();
        }
    }
    if let Some(p) = opts.get("port").and_then(|p| p.parse().ok()) {
        cfg.port = p;
    }
    if let Some(k) = opts.get("key") {
        cfg.key = k.clone();
    }
    if let Some(m) = opts.get("mic") {
        cfg.mic_device = m.clone();
    }
    if let Some(o) = opts.get("output") {
        cfg.output_device = o.clone();
    }
    if let Some(b) = opts.get("bitrate").and_then(|b| b.parse().ok()) {
        cfg.bitrate = b;
    }
    if let Some(j) = opts.get("jitter").and_then(|j| j.parse().ok()) {
        cfg.jitter_ms = j;
    }
    if opts.contains_key("follow") {
        cfg.follow_moonlight = true;
    }
    if opts.contains_key("no-follow") {
        cfg.follow_moonlight = false;
    }

    if verb == "firewall" {
        return match win::add_firewall_rule(cfg.port) {
            Ok(()) => {
                show(&format!("Firewall rule added for UDP {}.", cfg.port), console);
                0
            }
            Err(e) => {
                show(&format!("Could not add firewall rule: {e}"), console);
                1
            }
        };
    }
    if verb == "autostart" {
        let enable = !opts.contains_key("remove");
        return match win::set_autostart(enable) {
            Ok(lnk) if enable => {
                show(&format!("SideMic will start with Windows (in the tray) using the saved settings.\n\nShortcut: {lnk}"), console);
                0
            }
            Ok(_) => {
                show("SideMic will no longer start with Windows.", console);
                0
            }
            Err(e) => {
                show(&format!("Autostart failed: {e}"), console);
                1
            }
        };
    }

    if cfg.mode == "host" && (cfg.key.trim().is_empty() || opts.contains_key("new-key")) {
        cfg.key = Config::new_key();
        log::info("Generated a new pairing key");
    }
    let configured = cfg.mode == "host" || (cfg.mode == "client" && !cfg.host.trim().is_empty());
    if configured || verb == "host" || verb == "client" {
        if let Err(e) = cfg.save() {
            log::warn(format!("Could not save config: {e}"));
        }
    }

    // ---- console mode: no tray, no window ----
    if console {
        if !configured {
            show(&format!("Not set up yet.\n\n{USAGE}"), true);
            return 2;
        }
        let shared = state::Shared::new(cfg.clone());
        if let Err(e) = shared.start() {
            log::error(e.to_string());
            println!("SideMic could not start:\n\n{e}");
            return 1;
        }
        println!("{}", shared.status());
        if cfg.mode == "host" {
            println!("Pairing key (give this to the laptop): {}", cfg.key);
        }
        println!("Press Ctrl+C to stop.");
        win::install_ctrl_c_handler();
        let mut last = String::new();
        while !win::ctrl_c_pressed() {
            std::thread::sleep(Duration::from_secs(1));
            let s = shared.status();
            if s != last {
                println!("{s}");
                last = s;
            }
        }
        shared.stop();
        return 0;
    }

    // ---- normal mode: tray always, window on demand ----
    if !win::claim_single_instance("app") {
        show("SideMic is already running - look for it in the system tray (double-click to open the window).", false);
        return 1;
    }
    if cfg.mode == "host" {
        log::info(format!("Pairing key: {}", cfg.key));
    }
    let shared = state::Shared::new(cfg);
    if configured {
        shared.start_logged();
    }
    let Some(mut tray) = tray::Tray::new(shared.clone()) else {
        show("SideMic could not create its tray icon.", false);
        return 1;
    };

    // Open the window unless told to stay in the tray (autostart shortcut) or launched with a verb.
    let mut show_window = !opts.contains_key("minimized") && verb.is_empty();
    if !configured {
        show_window = true;
    }
    loop {
        if show_window {
            gui::run_window(shared.clone(), &mut tray);
            show_window = false;
        }
        if shared.exit.load(Ordering::SeqCst) {
            break;
        }
        tray.idle_loop();
        if shared.exit.load(Ordering::SeqCst) {
            break;
        }
        if shared.show_window.swap(false, Ordering::SeqCst) {
            show_window = true;
        }
    }
    shared.stop();
    0
}
