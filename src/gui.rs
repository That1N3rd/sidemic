//! The settings window (egui). Opened on demand from the tray; closing it keeps SideMic running.
//!
//! Windows: each time the window opens, `run_window` runs eframe until the window closes, then returns to the
//! tray's own message loop.
//! macOS: `run_app` runs eframe once for the whole life of the app and the window is only ever hidden. AppKit
//! has a single application run loop that winit owns, and the menu-bar icon needs that loop running all the
//! time, so handing control back and forth (as on Windows) would leave the icon dead while the window is shut.

use crate::config::Config;
use crate::state::Shared;
use crate::tray::{self, Tray};
use crate::{audio, log, platform};
use eframe::egui::{self, Color32, RichText};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub const AUTO: &str = "(automatic)";

/// The tray is borrowed from main() on Windows, and created and owned by the window on macOS.
enum TrayRef<'a> {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Borrowed(&'a mut Tray),
    #[cfg_attr(windows, allow(dead_code))]
    Owned(Box<Tray>),
}

impl TrayRef<'_> {
    fn poll(&mut self) {
        match self {
            TrayRef::Borrowed(t) => t.poll(),
            TrayRef::Owned(t) => t.poll(),
        }
    }
}

struct App<'a> {
    shared: Arc<Shared>,
    tray: TrayRef<'a>,
    /// macOS: frames left during which to keep the window hidden at startup (eframe shows the window after it
    /// paints the first frame, so a single Visible(false) is not enough).
    #[cfg_attr(windows, allow(dead_code))]
    hide_frames: u8,
    /// macOS: run the "window appeared" hook on the first frame.
    #[cfg_attr(windows, allow(dead_code))]
    first_frame: bool,
    cfg: Config,
    mics: Vec<String>,
    outputs: Vec<String>,
    show_key: bool,
    autostart: bool,
    notice: String,
    notice_is_error: bool,
}

impl<'a> App<'a> {
    fn new(shared: Arc<Shared>, tray: TrayRef<'a>, start_hidden: bool) -> Self {
        let cfg = shared.cfg.lock().unwrap().clone();
        let (mics, outputs) = audio::device_names();
        App {
            shared,
            tray,
            hide_frames: if start_hidden { 5 } else { 0 },
            first_frame: !start_hidden,
            cfg,
            mics,
            outputs,
            show_key: false,
            autostart: platform::autostart_enabled(),
            notice: String::new(),
            notice_is_error: false,
        }
    }

    /// Re-reads settings and devices, as if the window had just been opened (Windows rebuilds `App` each time).
    #[cfg_attr(windows, allow(dead_code))]
    fn refresh(&mut self) {
        self.cfg = self.shared.cfg.lock().unwrap().clone();
        let (m, o) = audio::device_names();
        self.mics = m;
        self.outputs = o;
        self.autostart = platform::autostart_enabled();
    }

    /// "Close" the window: really close it on Windows, hide it on macOS.
    fn hide(&mut self, ctx: &egui::Context) {
        if cfg!(target_os = "macos") {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            platform::on_window_hidden();
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn set_notice(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = text.into();
        self.notice_is_error = is_error;
    }

    fn save(&mut self) -> bool {
        if self.cfg.mode == "host" && self.cfg.key.trim().is_empty() {
            self.cfg.key = Config::new_key();
        }
        let mut shared_cfg = self.shared.cfg.lock().unwrap();
        self.cfg.path = shared_cfg.path.clone();
        *shared_cfg = self.cfg.clone();
        match shared_cfg.save() {
            Ok(()) => true,
            Err(e) => {
                drop(shared_cfg);
                self.set_notice(format!("Could not save settings: {e}"), true);
                false
            }
        }
    }

    fn start(&mut self) {
        if !self.save() {
            return;
        }
        match self.shared.start() {
            Ok(()) => self.set_notice("Running.", false),
            Err(e) => self.set_notice(e.to_string(), true),
        }
    }

    fn device_combo(ui: &mut egui::Ui, id: &str, current: &mut String, names: &[String]) {
        let label = if current.trim().is_empty() { AUTO.to_string() } else { current.clone() };
        egui::ComboBox::from_id_salt(id).width(360.0).selected_text(label).show_ui(ui, |ui| {
            if ui.selectable_label(current.trim().is_empty(), AUTO).clicked() {
                current.clear();
            }
            for n in names {
                if ui.selectable_label(current == n, n).clicked() {
                    *current = n.clone();
                }
            }
        });
    }

    fn host_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Host settings");
        ui.label("This PC receives the microphone and feeds it into a virtual mic that games and voice apps can select.");
        ui.add_space(4.0);
        egui::Grid::new("host-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Virtual mic output");
            ui.horizontal(|ui| {
                Self::device_combo(ui, "out", &mut self.cfg.output_device, &self.outputs);
                if ui.small_button("Refresh").on_hover_text("Refresh device list").clicked() {
                    let (m, o) = audio::device_names();
                    self.mics = m;
                    self.outputs = o;
                }
            });
            ui.end_row();

            ui.label("UDP port");
            ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535));
            ui.end_row();

            ui.label("Jitter cushion");
            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut self.cfg.jitter_ms, 20..=300).suffix(" ms"));
                ui.label("raise on Wi-Fi/Internet if you hear crackles");
            });
            ui.end_row();

            ui.label("Pairing key");
            ui.horizontal(|ui| {
                if self.cfg.key.trim().is_empty() {
                    self.cfg.key = Config::new_key();
                }
                ui.add(egui::TextEdit::singleline(&mut self.cfg.key).password(!self.show_key).desired_width(300.0).interactive(false));
                if ui.button(if self.show_key { "Hide" } else { "Show" }).clicked() {
                    self.show_key = !self.show_key;
                }
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(self.cfg.key.clone());
                    self.set_notice("Pairing key copied. Paste it into SideMic on the laptop.", false);
                }
                if ui.button("New key").on_hover_text("The laptop will need the new key too").clicked() {
                    self.cfg.key = Config::new_key();
                }
            });
            ui.end_row();

            ui.label("Firewall");
            if ui.button(platform::FIREWALL_LABEL).clicked() {
                match platform::add_firewall_rule(self.cfg.port) {
                    Ok(()) => self.set_notice(format!("Firewall now lets SideMic receive on UDP {}.", self.cfg.port), false),
                    Err(e) => self.set_notice(format!("Firewall: {e}"), true),
                }
            }
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.label(RichText::new(HOST_HINT).small().weak());
    }

    fn client_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Client settings");
        ui.label("This PC sends its microphone to the host while Moonlight is running.");
        ui.add_space(4.0);
        egui::Grid::new("client-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Host address");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.host).hint_text("IP or hostname of the streamed PC").desired_width(240.0));
                ui.label("port");
                ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535));
            });
            ui.end_row();

            ui.label("Pairing key");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.key).hint_text("paste the key from the host").password(!self.show_key).desired_width(300.0));
                if ui.button(if self.show_key { "Hide" } else { "Show" }).clicked() {
                    self.show_key = !self.show_key;
                }
            });
            ui.end_row();

            ui.label("Microphone");
            ui.horizontal(|ui| {
                Self::device_combo(ui, "mic", &mut self.cfg.mic_device, &self.mics);
                if ui.small_button("Refresh").on_hover_text("Refresh device list").clicked() {
                    let (m, o) = audio::device_names();
                    self.mics = m;
                    self.outputs = o;
                }
            });
            ui.end_row();

            ui.label("Bitrate");
            ui.add(egui::Slider::new(&mut self.cfg.bitrate, 16000..=96000).step_by(4000.0).custom_formatter(|v, _| format!("{} kbps", v / 1000.0)));
            ui.end_row();

            ui.label("When to send");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cfg.follow_moonlight, "Only while this process is running:");
                ui.add_enabled(self.cfg.follow_moonlight, egui::TextEdit::singleline(&mut self.cfg.moonlight_process).desired_width(120.0));
                ui.label(platform::EXE_SUFFIX);
            });
            ui.end_row();

            ui.label("Mute");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cfg.start_muted, "Start muted");
                if self.shared.client_running() {
                    let mut m = self.shared.muted();
                    if ui.checkbox(&mut m, "Muted right now").changed() {
                        self.shared.set_muted(m);
                    }
                }
            });
            ui.end_row();
        });
    }
}

impl eframe::App for App<'_> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tray.poll();
        let exiting = self.shared.exit.load(Ordering::SeqCst);
        if exiting {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        #[cfg(target_os = "macos")]
        {
            if self.hide_frames > 0 {
                self.hide_frames -= 1;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                platform::on_window_hidden();
            } else if std::mem::take(&mut self.first_frame) {
                platform::on_window_shown();
            }
            // The red close button (or Cmd+W) hides the window; only Quit in the menu-bar menu ends the app.
            if !exiting && ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide(ctx);
            }
            if self.shared.show_window.swap(false, Ordering::SeqCst) {
                self.hide_frames = 0;
                self.first_frame = false;
                self.refresh();
                platform::on_window_shown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let running = self.shared.running();
        let status = self.shared.status();

        egui::Panel::top("header").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("SideMic").size(24.0));
                ui.add_space(8.0);
                let color = if !running {
                    Color32::GRAY
                } else if status.starts_with("Error") || status.contains("no reply") || status.contains("silent") {
                    Color32::from_rgb(230, 160, 30)
                } else if status.starts_with("Receiving") || status.contains("connected") {
                    Color32::from_rgb(60, 180, 90)
                } else {
                    Color32::from_rgb(90, 150, 230)
                };
                let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 6.0, color);
                ui.label(RichText::new(&status).size(14.0));
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("footer").show(root, |ui| {
            ui.add_space(6.0);
            if !self.notice.is_empty() {
                let color = if self.notice_is_error { Color32::from_rgb(220, 70, 70) } else { Color32::from_rgb(60, 180, 90) };
                ui.label(RichText::new(&self.notice).color(color));
            }
            ui.horizontal(|ui| {
                if running {
                    if ui.button(RichText::new("Stop").size(16.0)).clicked() {
                        self.shared.stop();
                        self.set_notice("Stopped.", false);
                    }
                    if ui.button(RichText::new("Apply & restart").size(16.0)).clicked() {
                        self.start();
                    }
                } else if ui.button(RichText::new("Save & start").size(16.0)).clicked() {
                    self.start();
                }
                if ui.button("Save").clicked() && self.save() {
                    self.set_notice("Settings saved.", false);
                }
                ui.separator();
                if ui.checkbox(&mut self.autostart, platform::AUTOSTART_LABEL).changed() {
                    match platform::set_autostart(self.autostart) {
                        Ok(_) if self.autostart => self.set_notice(format!("SideMic will start at login (in the {}).", platform::TRAY_NAME), false),
                        Ok(_) => self.set_notice("SideMic will no longer start at login.", false),
                        Err(e) => {
                            self.autostart = !self.autostart;
                            self.set_notice(format!("Autostart: {e}"), true);
                        }
                    }
                }
                if ui.button("Open log").clicked() {
                    if let Some(p) = log::file_path() {
                        platform::open_path(&p.display().to_string());
                    }
                }
                if ui.button(format!("Hide to {}", platform::TRAY_NAME)).clicked() {
                    self.hide(&ctx);
                }
            });
            ui.label(RichText::new(CLOSE_HINT).small().weak());
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("This PC is the").strong());
                    ui.radio_value(&mut self.cfg.mode, "host".to_string(), "streamed PC (host) - receives the mic");
                    ui.radio_value(&mut self.cfg.mode, "client".to_string(), "laptop (client) - sends the mic");
                });
                ui.separator();
                match self.cfg.mode.as_str() {
                    "host" => self.host_section(ui),
                    "client" => self.client_section(ui),
                    _ => {
                        ui.add_space(12.0);
                        ui.label("Pick a role above to get started.");
                        ui.add_space(8.0);
                        ui.label(RichText::new("Host = the PC you stream games from (where Sunshine or Apollo runs).\nClient = the laptop you play on (where Moonlight runs).").weak());
                    }
                }
            });
        });
    }
}

#[cfg(windows)]
const HOST_HINT: &str = "Install VB-Cable once (vb-audio.com/Cable), then pick \"CABLE Output\" as the microphone in your game. \
    \"(automatic)\" finds VB-Cable or the Steam Streaming Microphone by itself.";
#[cfg(target_os = "macos")]
const HOST_HINT: &str = "Install BlackHole 2ch once (existential.audio/blackhole), then pick \"BlackHole 2ch\" as the microphone \
    in your app. \"(automatic)\" finds BlackHole by itself.";

#[cfg(windows)]
const CLOSE_HINT: &str = "Closing this window keeps SideMic running in the system tray. Use the tray menu's Exit to quit.";
#[cfg(target_os = "macos")]
const CLOSE_HINT: &str = "Closing this window keeps SideMic running in the menu bar. Use Quit SideMic in its menu to quit.";

fn native_options() -> eframe::NativeOptions {
    let icon = egui::IconData { rgba: tray::icon_rgba(64), width: 64, height: 64 };
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SideMic")
            .with_inner_size([640.0, 520.0])
            .with_min_inner_size([520.0, 420.0])
            .with_icon(Arc::new(icon)),
        renderer: eframe::Renderer::Glow,
        run_and_return: true,
        centered: true,
        ..Default::default()
    }
}

/// Windows: opens the settings window and blocks until it is closed. The tray keeps working meanwhile.
#[cfg(windows)]
pub fn run_window(shared: Arc<Shared>, tray: &mut Tray) {
    shared.show_window.store(false, Ordering::SeqCst);
    let shared2 = shared.clone();
    let result = eframe::run_native(
        "SideMic",
        native_options(),
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            shared2.set_waker(move || ctx.request_repaint());
            Ok(Box::new(App::new(shared2.clone(), TrayRef::Borrowed(tray), false)))
        }),
    );
    if let Err(e) = result {
        log::error(format!("Settings window failed: {e}"));
        platform::message_box(&format!("The settings window could not be opened:\n\n{e}\n\nSideMic keeps running in the tray; use sidemic.json or the command line to configure it."));
    }
}

/// macOS: runs the whole app (window + menu-bar icon) until Quit. Returns false if it could not start.
#[cfg(target_os = "macos")]
pub fn run_app(shared: Arc<Shared>, start_hidden: bool) -> bool {
    shared.show_window.store(false, Ordering::SeqCst);
    let shared2 = shared.clone();
    let result = eframe::run_native(
        "SideMic",
        native_options(),
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            shared2.set_waker(move || ctx.request_repaint());
            // The menu-bar icon has to be created once AppKit's run loop is up, which is the case by now.
            let tray = Tray::new(shared2.clone()).ok_or("could not create the menu-bar icon")?;
            Ok(Box::new(App::new(shared2.clone(), TrayRef::Owned(Box::new(tray)), start_hidden)))
        }),
    );
    match result {
        Ok(()) => true,
        Err(e) => {
            log::error(format!("SideMic window failed: {e}"));
            platform::message_box(&format!("SideMic could not open its window:\n\n{e}\n\nThe command line still works: sidemic help"));
            false
        }
    }
}
