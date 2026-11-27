//! macOS helpers: process check, alerts, single instance, clipboard, login item, firewall, Dock icon.
//!
//! The Windows build talks to Win32 directly; here most things are either a libc call or one of the small
//! command-line tools every Mac ships with (open, pbcopy, osascript), which keeps the dependency list short.

use std::ffi::c_void;
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSString;

pub const EXE_SUFFIX: &str = "";
pub const AUTOSTART_LABEL: &str = "Open at login";
pub const TRAY_NAME: &str = "menu bar";
pub const FIREWALL_LABEL: &str = "Allow incoming connections in the macOS firewall";
pub const MIC_PERMISSION_HINT: &str =
    "allow SideMic (or your terminal) in System Settings > Privacy & Security > Microphone";

/// Launch agent label; also the bundle identifier in Info.plist.
const AGENT_LABEL: &str = "io.github.that1n3rd.sidemic";

/// A Mac process always has a usable stdout (the terminal, or /dev/null when started from Finder).
pub fn enable_console() -> bool {
    true
}

static CTRL_C: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    // Only an atomic store: that is all a signal handler may safely do.
    CTRL_C.store(true, Ordering::SeqCst);
}

pub fn install_ctrl_c_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

pub fn ctrl_c_pressed() -> bool {
    CTRL_C.load(Ordering::SeqCst)
}

/// Some(true) if a process named `name` is running (case-insensitive, ".app" ignored), Some(false) if not,
/// None if the process list could not be read. Uses libproc, the same source Activity Monitor reads.
pub fn process_running(name: &str) -> Option<bool> {
    let want = name.trim().trim_end_matches(".app").to_lowercase();
    unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if count <= 0 {
            return None;
        }
        // Leave headroom: processes can start between the two calls.
        let mut pids: Vec<libc::c_int> = vec![0; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
        let n = libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, bytes);
        if n <= 0 {
            return None;
        }
        let mut buf = [0u8; 256];
        for &pid in &pids[..(n as usize).min(pids.len())] {
            if pid <= 0 {
                continue;
            }
            let len = libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32);
            if len > 0 && String::from_utf8_lossy(&buf[..len as usize]).to_lowercase() == want {
                return Some(true);
            }
        }
        Some(false)
    }
}

/// Shows a modal alert. Must run on the main thread (it always does in SideMic); falls back to stderr if not.
pub fn message_box(text: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("{text}");
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("SideMic"));
    alert.setInformativeText(&NSString::from_str(text));
    // A menu-bar app is usually not the active app; without this the alert can open behind other windows.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    alert.runModal();
}

/// Returns false if another SideMic already holds the lock file. The file descriptor is intentionally leaked so
/// the lock lives exactly as long as the process (the kernel releases it on exit, even after a crash).
pub fn claim_single_instance(name: &str) -> bool {
    let path = crate::config::data_dir().join(format!(".sidemic-{name}.lock"));
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) else {
        return true; // can't tell, so don't block startup
    };
    let fd = file.into_raw_fd();
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Opens a file with its default app (Console for .log, a text editor for .json).
pub fn open_path(path: &str) {
    let _ = Command::new("/usr/bin/open").arg(path).spawn();
}

pub fn set_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("/usr/bin/pbcopy").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    let wrote = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()).is_ok()).unwrap_or(false);
    wrote && child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Quotes `s` as an AppleScript string literal.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The macOS application firewall is per app, not per port (and off by default). If it is on, this adds SideMic
/// to its allow list. Asks for an administrator password once.
pub fn add_firewall_rule(_port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = applescript_string(&exe.display().to_string());
    let fw = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    let script = format!(
        "do shell script \"{fw} --add \" & quoted form of {exe} & \" && {fw} --unblockapp \" & quoted form of {exe} \
         with administrator privileges"
    );
    let out = Command::new("/usr/bin/osascript").args(["-e", &script]).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.contains("-128") { "cancelled".into() } else { err })
    }
}

fn agent_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
}

/// The enclosing SideMic.app if this binary is inside one (…/SideMic.app/Contents/MacOS/sidemic).
pub fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension().map(|e| e == "app").unwrap_or(false);
    is_bundle.then(|| bundle.to_path_buf())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Creates or removes a per-user launch agent that starts SideMic in the menu bar at login.
pub fn set_autostart(enable: bool) -> Result<String, String> {
    let path = agent_path().ok_or("HOME is not set")?;
    if !enable {
        let _ = std::fs::remove_file(&path);
        return Ok(path.display().to_string());
    }
    // Inside an app bundle, go through `open` so macOS treats it as the app (microphone permission, single
    // instance). A bare binary (cargo build) is started directly.
    let args: Vec<String> = match app_bundle() {
        Some(app) => vec!["/usr/bin/open".into(), "-g".into(), "-a".into(), app.display().to_string(), "--args".into(), "--minimized".into()],
        None => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            vec![exe.display().to_string(), "--minimized".into()]
        }
    };
    let args_xml: String = args.iter().map(|a| format!("        <string>{}</string>\n", xml_escape(a))).collect();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, plist).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

pub fn autostart_enabled() -> bool {
    agent_path().map(|p| p.exists()).unwrap_or(false)
}

/// Window visible: show a Dock icon and come to the front, like a normal app.
pub fn on_window_shown() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// Window hidden: drop the Dock icon and live in the menu bar only.
pub fn on_window_hidden() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}
