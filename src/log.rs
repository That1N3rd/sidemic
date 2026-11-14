//! Tiny thread-safe logger: console (when enabled) + sidemic.log + an in-memory tail.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct State {
    file: Option<File>,
    path: Option<PathBuf>,
    console: bool,
    recent: VecDeque<String>,
}

static LOG: Mutex<State> = Mutex::new(State { file: None, path: None, console: false, recent: VecDeque::new() });

pub fn open(path: &Path) {
    let mut st = LOG.lock().unwrap();
    // Keep the log from growing forever.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 2_000_000 {
            let _ = std::fs::remove_file(path);
        }
    }
    st.file = OpenOptions::new().create(true).append(true).open(path).ok();
    st.path = Some(path.to_path_buf());
}

pub fn set_console(enabled: bool) {
    LOG.lock().unwrap().console = enabled;
}

pub fn file_path() -> Option<PathBuf> {
    LOG.lock().unwrap().path.clone()
}

pub fn recent_text() -> String {
    LOG.lock().unwrap().recent.iter().cloned().collect::<Vec<_>>().join("\n")
}

pub fn info(msg: impl AsRef<str>) { write("INFO", msg.as_ref()); }
pub fn warn(msg: impl AsRef<str>) { write("WARN", msg.as_ref()); }
pub fn error(msg: impl AsRef<str>) { write("ERR ", msg.as_ref()); }

fn write(level: &str, msg: &str) {
    let line = format!("{} {} {}", chrono::Local::now().format("%H:%M:%S%.3f"), level, msg);
    let mut st = LOG.lock().unwrap();
    if st.console {
        println!("{line}");
    }
    if let Some(f) = st.file.as_mut() {
        let _ = writeln!(f, "{line}");
    }
    st.recent.push_back(line);
    while st.recent.len() > 200 {
        st.recent.pop_front();
    }
}
