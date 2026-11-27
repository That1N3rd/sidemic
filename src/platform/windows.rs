//! Windows helpers: console attach, process check, message pump, message box, shell commands.

use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MessageBoxW, PeekMessageW, TranslateMessage, MB_ICONINFORMATION, MB_OK, MSG, PM_REMOVE,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Makes println! visible for a windows-subsystem exe: reuse the parent console, or open a new one.
/// Returns true if some output channel exists (including redirected stdout).
pub fn enable_console() -> bool {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            return true; // stdout already redirected to a file/pipe or inherited
        }
        AttachConsole(ATTACH_PARENT_PROCESS) != 0 || AllocConsole() != 0
    }
}

static CTRL_C: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
    CTRL_C.store(true, Ordering::SeqCst);
    1
}

pub fn install_ctrl_c_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

pub fn ctrl_c_pressed() -> bool {
    CTRL_C.load(Ordering::SeqCst)
}

/// Some(true) if a process named `name` (with or without .exe) is running, Some(false) if not,
/// None if the process list could not be read (caller should keep its previous answer). Case-insensitive.
pub fn process_running(name: &str) -> Option<bool> {
    let want = name.trim_end_matches(".exe").trim_end_matches(".EXE").to_lowercase();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return None;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
                let exe = String::from_utf16_lossy(&pe.szExeFile[..len]).to_lowercase();
                if exe.trim_end_matches(".exe") == want {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        Some(found)
    }
}

/// Process any pending window messages (needed for the tray icon). Non-blocking.
pub fn pump_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn message_box(text: &str) {
    let t = wide(text);
    let c = wide("SideMic");
    unsafe {
        MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

/// Returns false if another instance already holds the named mutex. The handle is intentionally leaked
/// so the mutex lives as long as the process.
pub fn claim_single_instance(name: &str) -> bool {
    let n = wide(&format!("Local\\SideMic.{name}"));
    unsafe {
        let h = CreateMutexW(null(), 0, n.as_ptr());
        if h.is_null() {
            return true;
        }
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Opens a file with its default application (notepad for .json/.log, etc.).
pub fn open_path(path: &str) {
    let _ = Command::new("explorer.exe").arg(path).spawn();
}

fn powershell(script: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", script]).status()
}

/// Copies text to the clipboard. Only used for the base64 pairing key, which has no quote characters.
pub fn set_clipboard(text: &str) -> bool {
    let safe: String = text.chars().filter(|c| c.is_ascii_alphanumeric() || "+/=".contains(*c)).collect();
    powershell(&format!("Set-Clipboard -Value '{safe}'")).map(|s| s.success()).unwrap_or(false)
}

/// Adds an inbound UDP firewall rule for this exe. Triggers one UAC prompt.
pub fn add_firewall_rule(port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args = format!(
        "advfirewall firewall add rule name=\"SideMic (UDP {port})\" dir=in action=allow protocol=UDP localport={port} program=\"{}\"",
        exe.display()
    );
    let script = format!(
        "$p = Start-Process -FilePath netsh -Verb RunAs -Wait -PassThru -ArgumentList '{}'; exit $p.ExitCode",
        args.replace('\'', "''")
    );
    match powershell(&script) {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("netsh exited with code {}", s.code().unwrap_or(-1))),
        Err(e) => Err(e.to_string()),
    }
}

/// Creates or removes a shortcut in the user's Startup folder.
pub fn set_autostart(enable: bool) -> Result<String, String> {
    let appdata = std::env::var("APPDATA").map_err(|e| e.to_string())?;
    let lnk = format!("{appdata}\\Microsoft\\Windows\\Start Menu\\Programs\\Startup\\SideMic.lnk");
    if !enable {
        let _ = std::fs::remove_file(&lnk);
        return Ok(lnk);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{lnk}'); $s.TargetPath = '{exe}'; $s.Arguments = '--minimized'; \
         $s.WorkingDirectory = '{dir}'; $s.IconLocation = '{exe},0'; $s.Description = 'SideMic - microphone passthrough for game streaming'; $s.Save()",
        exe = exe.display()
    );
    match powershell(&script) {
        Ok(s) if s.success() => Ok(lnk),
        Ok(s) => Err(format!("powershell exited with code {}", s.code().unwrap_or(-1))),
        Err(e) => Err(e.to_string()),
    }
}

/// True if the Startup-folder shortcut exists.
pub fn autostart_enabled() -> bool {
    std::env::var("APPDATA")
        .map(|a| std::path::Path::new(&format!(r"{a}\Microsoft\Windows\Start Menu\Programs\Startup\SideMic.lnk")).exists())
        .unwrap_or(false)
}

// ---- wording and hooks the shared code uses; macos.rs has the Mac versions ----

/// Appended to the Moonlight process name in the UI ("Moonlight" + ".exe").
pub const EXE_SUFFIX: &str = ".exe";
/// Label of the autostart checkbox.
pub const AUTOSTART_LABEL: &str = "Start with Windows";
/// Where the tray icon lives, for user-facing text.
pub const TRAY_NAME: &str = "system tray";
/// Label of the host's firewall button.
pub const FIREWALL_LABEL: &str = "Allow UDP port through Windows Firewall";
/// What to check when the microphone delivers nothing but digital silence.
pub const MIC_PERMISSION_HINT: &str = "check Settings > Privacy & security > Microphone";

/// Called when the settings window appears / disappears. Nothing to do on Windows.
#[allow(dead_code)] // only the macOS window code calls this
pub fn on_window_shown() {}
pub fn on_window_hidden() {}
