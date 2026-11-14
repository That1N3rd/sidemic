//! State shared between the settings window, the tray icon and the running service.

use crate::client::Client;
use crate::config::Config;
use crate::host::Host;
use crate::log;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub enum Service {
    Host(Arc<Host>),
    Client(Arc<Client>),
}

pub struct Shared {
    pub cfg: Mutex<Config>,
    service: Mutex<Option<Service>>,
    /// Tray asked for the window to be shown.
    pub show_window: AtomicBool,
    /// Someone asked the whole program to quit.
    pub exit: AtomicBool,
    /// Set by the window: pokes the GUI thread so it notices show_window / exit promptly.
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    last_error: Mutex<String>,
}

impl Shared {
    pub fn new(cfg: Config) -> Arc<Shared> {
        Arc::new(Shared {
            cfg: Mutex::new(cfg),
            service: Mutex::new(None),
            show_window: AtomicBool::new(false),
            exit: AtomicBool::new(false),
            wake: Mutex::new(None),
            last_error: Mutex::new(String::new()),
        })
    }

    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.wake.lock().unwrap() = Some(Box::new(f));
    }

    pub fn wake(&self) {
        if let Some(f) = self.wake.lock().unwrap().as_ref() {
            f();
        }
    }

    pub fn request_exit(&self) {
        self.exit.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn request_show(&self) {
        self.show_window.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn running(&self) -> bool {
        self.service.lock().unwrap().is_some()
    }

    pub fn pairing_key(&self) -> String {
        self.cfg.lock().unwrap().key.clone()
    }

    pub fn last_error(&self) -> String {
        self.last_error.lock().unwrap().clone()
    }

    /// Starts the service for the current config (stopping any running one first).
    pub fn start(&self) -> Result<()> {
        self.stop();
        let cfg = self.cfg.lock().unwrap().clone();
        let svc = match cfg.mode.as_str() {
            "host" => Service::Host(Host::start(cfg)?),
            "client" => Service::Client(Client::start(cfg)?),
            _ => anyhow::bail!("Choose whether this PC is the host or the client first."),
        };
        *self.service.lock().unwrap() = Some(svc);
        self.last_error.lock().unwrap().clear();
        Ok(())
    }

    pub fn start_logged(&self) -> bool {
        match self.start() {
            Ok(()) => true,
            Err(e) => {
                log::error(e.to_string());
                *self.last_error.lock().unwrap() = e.to_string();
                false
            }
        }
    }

    pub fn stop(&self) {
        if let Some(svc) = self.service.lock().unwrap().take() {
            match svc {
                Service::Host(h) => h.shutdown(),
                Service::Client(c) => c.shutdown(),
            }
        }
    }

    pub fn status(&self) -> String {
        match self.service.lock().unwrap().as_ref() {
            Some(Service::Host(h)) => h.status(),
            Some(Service::Client(c)) => c.status(),
            None => {
                let err = self.last_error();
                if !err.is_empty() {
                    format!("Stopped - {err}")
                } else {
                    "Stopped".into()
                }
            }
        }
    }

    /// True when a client is running (the only time mute means anything).
    pub fn client_running(&self) -> bool {
        matches!(self.service.lock().unwrap().as_ref(), Some(Service::Client(_)))
    }

    pub fn muted(&self) -> bool {
        match self.service.lock().unwrap().as_ref() {
            Some(Service::Client(c)) => c.muted(),
            _ => false,
        }
    }

    pub fn set_muted(&self, m: bool) {
        if let Some(Service::Client(c)) = self.service.lock().unwrap().as_ref() {
            c.set_muted(m);
        }
    }
}
