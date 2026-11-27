//! OS-specific glue. Everything else in SideMic (audio, protocol, GUI, tray) is shared; this module is the only
//! place that knows whether it runs on Windows or macOS. Both backends expose the same set of functions.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("SideMic supports Windows and macOS only.");
