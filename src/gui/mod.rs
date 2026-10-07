use crate::{app::{AppContext, ConnectionStatus}, config::Mode};
use eframe::egui;
use std::time::Instant;
use tray_icon::{menu::{Menu, MenuEvent, MenuItem}, Icon, TrayIconBuilder};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page { Connection, Diagnostics, Statistics, Logs, Settings }

pub fn run(ctx: AppContext, start_hidden: bool) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 720.0]).with_min_inner_size([680.0, 480.0]).with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native("TeleRoute", options, Box::new(move |cc| Ok(Box::new(TeleRouteApp::new(cc, ctx.clone())?))))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

struct TrayState { _tray: tray_icon::TrayIcon, connect: MenuItem, disconnect: MenuItem, open: MenuItem, test: MenuItem, exit: MenuItem }

struct TeleRouteApp {
    ctx: AppContext,
    page: Page,
    last_refresh: Instant,
    diagnostics: Option<crate::diagnostics::DiagnosticReport>,
    diagnostics_rx: Option<std::sync::mpsc::Receiver<crate::diagnostics::DiagnosticReport>>,
    logs_cache: String,
    tray: Option<TrayState>,
}

impl TeleRouteApp {
    fn new(_cc: &eframe::CreationContext<'_>, ctx: AppContext) -> anyhow::Result<Self> {
        let tray = build_tray().ok();
        Ok(Self { ctx, page: Page::Connection, last_refresh: Instant::now(), diagnostics: None, diagnostics_rx: None, logs_cache: String::new(), tray })
    }

    fn status_text(&self) -> &'static str {
        match self.ctx.status().status { ConnectionStatus::Connected => "CONNECTED", ConnectionStatus::Connecting => "CONNECTING", ConnectionStatus::Error => "ERROR", ConnectionStatus::Disconnected => "DISCONNECTED" }
    }

    fn mode_text(&self) -> String { format!("{:?}", self.ctx.config.read().routing.mode) }

    fn refresh_logs(&mut self) {
        let mut files: Vec<_> = std::fs::read_dir(crate::config::AppConfig::logs_dir()).ok().into_iter().flatten().flatten().collect();
        files.sort_by_key(|e| e.file_name());
        if let Some(path) = files.last().map(|e| e.path()) { if let Ok(text) = std::fs::read_to_string(path) { self.logs_cache = text.lines().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"); } }
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        let rx = MenuEvent::receiver();
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    if self.tray.as_ref().map(|t| event.id() == &t.connect.id()).unwrap_or(false) { self.ctx.connect(); }
                    else if self.tray.as_ref().map(|t| event.id() == &t.disconnect.id()).unwrap_or(false) { self.ctx.disconnect(); }
                    else if self.tray.as_ref().map(|t| event.id() == &t.open.id()).unwrap_or(false) { ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true)); }
                    else if self.tray.as_ref().map(|t| event.id() == &t.test.id()).unwrap_or(false) { self.page = Page::Diagnostics; self.start_diagnostics(); }
                    else if self.tray.as_ref().map(|t| event.id() == &t.exit.id()).unwrap_or(false) { self.ctx.disconnect(); ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                }
                Err(_) => break,
            }
        }
    }

    fn start_diagnostics(&mut self) {
        let ctx = self.ctx.clone();
        let task_ctx = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.spawn(async move { let result = task_ctx.run_diagnostics().await; let _ = tx.send(result); });
        self.diagnostics_rx = Some(rx);
    }
}

impl eframe::App for TeleRouteApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_tray(&ctx);
        if let Some(rx) = &self.diagnostics_rx {
            if let Ok(result) = rx.try_recv() {
                self.diagnostics = Some(result);
                self.diagnostics_rx = None;
            }
        }
        if self.last_refresh.elapsed().as_secs_f32() > 0.5 {
            self.last_refresh = Instant::now();
            if self.page == Page::Logs { self.refresh_logs(); }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.ctx.config.read().autostart.minimize_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.ctx.disconnect();
                return;
            }
        }

        let narrow = ui.available_width() < 820.0;

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("TeleRoute");
                ui.separator();
                ui.label("Telegram transport + low-latency UDP routing");
            });
        });

        if !narrow {
            egui::Panel::left("nav").min_size([178.0, 0.0].into()).show(ui, |ui| {
                self.navigation(ui, false);
            });
        } else {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::horizontal().id_salt("compact-navigation").show(ui, |ui| {
                    self.navigation(ui, true);
                });
            });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            match self.page {
                Page::Connection => self.connection_page(ui),
                Page::Diagnostics => self.diagnostics_page(ui),
                Page::Statistics => self.statistics_page(ui),
                Page::Logs => self.logs_page(ui),
                Page::Settings => self.settings_page(ui),
            }
        });
    }
}

impl TeleRouteApp {
    fn navigation(&mut self, ui: &mut egui::Ui, compact: bool) {
        if !compact {
            ui.heading("Navigation");
            ui.separator();
        }
        let items = [
            (Page::Connection, "Connection"),
            (Page::Diagnostics, "Diagnostics"),
            (Page::Statistics, "Statistics"),
            (Page::Logs, "Logs"),
            (Page::Settings, "Settings"),
        ];
        if compact {
            ui.horizontal(|ui| {
                for (page, name) in items {
                    ui.selectable_value(&mut self.page, page, name);
                }
                ui.separator();
                if ui.button("Connect").clicked() { self.ctx.connect(); }
                if ui.button("Disconnect").clicked() { self.ctx.disconnect(); }
            });
        } else {
            for (page, name) in items {
                ui.add_sized([ui.available_width(), 32.0], egui::Button::selectable(self.page == page, name))
                    .clicked()
                    .then(|| self.page = page);
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let width = ui.available_width();
                ui.add_sized([width, 34.0], egui::Button::new("Connect")).clicked().then(|| self.ctx.connect());
            });
            ui.horizontal(|ui| {
                let width = ui.available_width();
                ui.add_sized([width, 34.0], egui::Button::new("Disconnect")).clicked().then(|| self.ctx.disconnect());
            });
        }
    }

    fn connection_page(&mut self, ui: &mut egui::Ui) {
        let t = self.ctx.status(); let s = self.ctx.snapshot();
        let route_transport = s.current_transport.to_string();
        let route_dc = s.current_dc.map(|v| v.to_string()).unwrap_or_else(|| t.dc.clone());
        ui.heading("Connection");
        ui.add_space(10.0);
        let cards = [
            ("Telegram", self.status_text().to_owned()),
            ("Mode", self.mode_text()),
            ("Transport", route_transport),
            ("DC", route_dc),
            ("Ping", s.ping_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into())),
            ("UDP", t.udp.clone()),
            ("Calls", t.calls.clone()),
        ];
        let columns = ((ui.available_width() / 180.0).floor() as usize).clamp(1, 4);
        ui.spacing_mut().item_spacing.y = 8.0;
        for row in cards.chunks(columns) {
            ui.columns(row.len(), |cols| {
                for (i, (title, value)) in row.iter().enumerate() {
                    responsive_card(&mut cols[i], title, value);
                }
            });
        }
        ui.add_space(16.0);
        ui.label(format!("SOCKS5: 127.0.0.1:{}", self.ctx.config.read().proxy.port));
        ui.label(format!("TUN: {}", t.tun));
        ui.label(format!("Active connections: {}", s.active_connections));
        ui.label(format!("↑ {}  ↓ {}", human_bytes(s.bytes_up), human_bytes(s.bytes_down)));
        ui.add_space(20.0);
        let button = if t.status == ConnectionStatus::Connected { "DISCONNECT" } else { "CONNECT" };
        let button_width = ui.available_width().min(360.0);
        ui.add_sized([button_width, 58.0], egui::Button::new(button)).clicked().then(|| {
            if t.status == ConnectionStatus::Connected { self.ctx.disconnect(); } else { self.ctx.connect(); }
        });
        if !t.error.is_empty() { ui.colored_label(egui::Color32::from_rgb(210,70,70), &t.error); }
        ui.separator();
        ui.label("Calls readiness is transport-level until a real Telegram Desktop voice/video call is completed end-to-end.");
    }

    fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Diagnostics");
        if ui.button("Run full test").clicked() { self.start_diagnostics(); }
        if let Some(r) = &self.diagnostics {
            egui::Grid::new("diagnostics-grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in [("DNS", &r.dns), ("Telegram TCP", &r.telegram_tcp), ("WebSocket", &r.websocket), ("TCP fallback", &r.tcp_fallback), ("UDP", &r.udp), ("TUN", &r.tun), ("Call transport", &r.call_transport)] {
                    ui.strong(k);
                    ui.label(v);
                    ui.end_row();
                }
            });
        }
        else { ui.label("No diagnostic run yet."); }
    }

    fn statistics_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Statistics");
        let s = self.ctx.snapshot();
        let values = [
            ("Connections", s.connections.to_string()),
            ("Active", s.active_connections.to_string()),
            ("Bytes up", human_bytes(s.bytes_up)),
            ("Bytes down", human_bytes(s.bytes_down)),
            ("Packets up", s.packets_up.to_string()),
            ("Packets down", s.packets_down.to_string()),
            ("Reconnects", s.reconnects.to_string()),
            ("WSS success", format!("{:.1}%", s.ws_success_rate)),
            ("TCP fallbacks", s.tcp_fallbacks.to_string()),
            ("UDP sessions", s.udp_sessions.to_string()),
            ("UDP blocked", s.udp_blocked.to_string()),
            ("Call bytes up", human_bytes(s.call_bytes_up)),
            ("Call bytes down", human_bytes(s.call_bytes_down)),
            ("Call jitter", s.call_jitter_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into())),
            ("Call packet loss", "—".into()),
        ];
        let columns = ((ui.available_width() / 220.0).floor() as usize).clamp(1, 4);
        for row in values.chunks(columns) {
            ui.columns(row.len(), |cols| {
                for (i, (title, value)) in row.iter().enumerate() {
                    responsive_card(&mut cols[i], title, value);
                }
            });
        }
    }

    fn logs_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Logs"); ui.horizontal(|ui| { if ui.button("Refresh").clicked() { self.refresh_logs(); } if ui.button("Clear").clicked() { self.logs_cache.clear(); } if ui.button("Open logs folder").clicked() { let _ = std::process::Command::new("explorer").arg(crate::config::AppConfig::logs_dir()).spawn(); } });
        egui::ScrollArea::vertical().show(ui, |ui| { ui.monospace(&self.logs_cache); });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings"); let mut cfg = self.ctx.config.write();
        ui.collapsing("General", |ui| { ui.checkbox(&mut cfg.autostart.start_with_windows,"Start with Windows"); ui.checkbox(&mut cfg.autostart.start_connected,"Start connected"); ui.checkbox(&mut cfg.autostart.start_minimized,"Start minimized"); ui.checkbox(&mut cfg.autostart.minimize_to_tray,"Minimize to tray"); });
        ui.collapsing("Proxy", |ui| { ui.horizontal(|ui| { ui.label("Host"); ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.proxy.bind)); }); ui.add(egui::Slider::new(&mut cfg.proxy.port, 1..=65535).text("Port")); });
        ui.collapsing("WebSocket", |ui| { ui.add(egui::Slider::new(&mut cfg.timeouts.connect_ms, 500..=15000).text("Connect timeout ms")); ui.add(egui::Slider::new(&mut cfg.timeouts.reconnect_ms, 100..=10000).text("Reconnect delay ms")); ui.label("Endpoint templates are configured in config.toml."); });
        ui.collapsing("Routing", |ui| { for mode in [Mode::Proxy,Mode::Calls,Mode::Full] { ui.radio_value(&mut cfg.routing.mode, mode, format!("{mode:?}")); } ui.checkbox(&mut cfg.routing.telegram_only,"Telegram-only UDP routing"); ui.checkbox(&mut cfg.routing.direct_udp_fallback,"Direct UDP fallback"); ui.checkbox(&mut cfg.routing.relay_udp_fallback,"QUIC relay fallback"); ui.checkbox(&mut cfg.routing.route_all_traffic,"Route all traffic (requires a future full TCP userspace stack; disabled now)"); });
        ui.collapsing("TUN", |ui| { ui.checkbox(&mut cfg.tun.enabled,"Enable TUN"); ui.add(egui::Slider::new(&mut cfg.tun.mtu, 576..=1500).text("MTU")); ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.tun.adapter_name)); ui.label(format!("Telegram UDP CIDRs: {}",cfg.tun.telegram_udp_cidrs.join(", "))); });
        ui.collapsing("Logging", |ui| { ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.logging.level)); ui.add(egui::Slider::new(&mut cfg.logging.keep_files,1..=20).text("Retained files")); });
        if ui.button("Save settings").clicked() { let _ = self.ctx.save_config(); }
    }
}

fn responsive_card(ui: &mut egui::Ui, title: &str, value: &str) {
    let width = ui.available_width().max(1.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_size([width, 72.0].into());
        ui.weak(title);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(value).strong().size(18.0));
    });
}
fn human_bytes(n: u64) -> String { const U:[&str;4]=["B","KB","MB","GB"]; let mut v=n as f64; let mut i=0; while v>=1024.0 && i<3 {v/=1024.0;i+=1;} format!("{v:.1} {}",U[i]) }
fn build_tray() -> anyhow::Result<TrayState> {
    let menu = Menu::new(); let connect = MenuItem::new("Connect",true,None); let disconnect=MenuItem::new("Disconnect",true,None); let open=MenuItem::new("Open",true,None); let test=MenuItem::new("Test",true,None); let exit=MenuItem::new("Exit",true,None);
    menu.append_items(&[&connect,&disconnect,&open,&test,&exit])?;
    let rgba: Vec<u8> = std::iter::repeat([0x20u8, 0x95u8, 0xffu8, 0xffu8]).take(16*16).flatten().collect();
    let icon = Icon::from_rgba(rgba,16,16)?;
    let tray=TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("TeleRoute").with_icon(icon).build()?;
    Ok(TrayState{_tray:tray,connect,disconnect,open,test,exit})
}
