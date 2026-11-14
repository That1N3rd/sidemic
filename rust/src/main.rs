//! MicBridge: send your laptop's microphone to the PC you stream from (Sunshine/Apollo + Moonlight).
//! A sidecar, not a fork, so Moonlight/Sunshine updates can't break it.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod client;
mod config;
mod host;
mod log;
mod protocol;
mod tray;
mod win;

use config::Config;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
MicBridge - send your laptop's microphone to the PC you stream from (Sunshine/Apollo + Moonlight).
Independent of Moonlight and Sunshine, so their updates never break it.

On the streamed PC (host):      micbridge host [--port 48020] [--output \"CABLE Input\"]
On the laptop (client):         micbridge client --host <host-ip> --key <key-from-host> [--mic \"name\"]
List audio devices:             micbridge devices
Allow UDP port in firewall:     micbridge firewall        (host only, asks for admin)
Start with Windows:             micbridge autostart       (micbridge autostart --remove to undo)

Options:
  --port N          UDP port (default 48020, same on both ends)
  --key K           shared key; host generates one on first run (tray menu > Copy pairing key)
  --new-key         host: generate a fresh key
  --mic NAME        client: part of the microphone's name (default: Windows default mic)
  --output NAME     host: part of the playback device name that feeds the virtual mic
  --bitrate N       Opus bits/s (default 40000)
  --jitter MS       host: jitter cushion (default 60)
  --follow / --no-follow   client: only send while Moonlight.exe runs (default: follow)
  --console         run in the terminal instead of the tray
  --config PATH     use a different micbridge.json

Settings are saved to micbridge.json, so after the first run you can just double-click micbridge.exe.
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
    log::open(&log_dir.join("micbridge.log"));
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
                show(&format!("MicBridge will start with Windows using the settings in micbridge.json.\n\nShortcut: {lnk}"), console);
                0
            }
            Ok(_) => {
                show("MicBridge will no longer start with Windows.", console);
                0
            }
            Err(e) => {
                show(&format!("Autostart failed: {e}"), console);
                1
            }
        };
    }

    if cfg.mode != "host" && cfg.mode != "client" {
        show(&format!("Not set up yet.\n\n{USAGE}"), console);
        return 2;
    }

    if cfg.mode == "host" && (cfg.key.trim().is_empty() || opts.contains_key("new-key")) {
        cfg.key = Config::new_key();
        log::info("Generated a new pairing key");
    }
    if let Err(e) = cfg.save() {
        log::warn(format!("Could not save config: {e}"));
    }

    if !console && !win::claim_single_instance(&cfg.mode) {
        show(&format!("MicBridge ({}) is already running - look for it in the system tray.", cfg.mode), false);
        return 1;
    }

    // Start the service.
    enum Service {
        Host(std::sync::Arc<host::Host>),
        Client(std::sync::Arc<client::Client>),
    }
    let service = if cfg.mode == "host" {
        host::Host::start(cfg.clone()).map(Service::Host)
    } else {
        client::Client::start(cfg.clone()).map(Service::Client)
    };
    let service = match service {
        Ok(s) => s,
        Err(e) => {
            log::error(e.to_string());
            show(
                &format!("MicBridge could not start:\n\n{e}\n\nLog: {}", log::file_path().map(|p| p.display().to_string()).unwrap_or_default()),
                console,
            );
            return 1;
        }
    };
    let status = || match &service {
        Service::Host(h) => h.status(),
        Service::Client(c) => c.status(),
    };
    let shutdown = || match &service {
        Service::Host(h) => h.shutdown(),
        Service::Client(c) => c.shutdown(),
    };
    if cfg.mode == "host" {
        log::info(format!("Pairing key: {}", cfg.key));
    }

    if console {
        println!("{}", status());
        if cfg.mode == "host" {
            println!("Pairing key (give this to the laptop): {}", cfg.key);
        }
        println!("Press Ctrl+C to stop.");
        win::install_ctrl_c_handler();
        let mut last = String::new();
        while !win::ctrl_c_pressed() {
            std::thread::sleep(Duration::from_secs(1));
            let s = status();
            if s != last {
                println!("{s}");
                last = s;
            }
        }
        shutdown();
        return 0;
    }

    let mute: Option<(Box<dyn Fn(bool)>, Box<dyn Fn() -> bool>)> = match &service {
        Service::Client(c) => {
            let c1 = c.clone();
            let c2 = c.clone();
            Some((Box::new(move |m| c1.set_muted(m)), Box::new(move || c2.muted())))
        }
        Service::Host(_) => None,
    };
    tray::run(tray::TrayOptions {
        title: if cfg.mode == "host" { "Host (receiving mic)" } else { "Client (sending mic)" },
        status: Box::new(status),
        mute,
        pairing_key: if cfg.mode == "host" { Some(cfg.key.clone()) } else { None },
        config_path: &cfg_path,
    });
    shutdown();
    0
}
