use crate::{app::{AppContext, ConnectionStatus}, config::{Mode, TelegramFrontend}};
use eframe::egui;
use std::time::Instant;
use tray_icon::{menu::{Menu, MenuItem}, Icon, TrayIconBuilder};

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

#[derive(Clone, Copy)]
enum TrayAction { Open, Test, ConfigureTelegram }

struct TrayState {
    _tray: tray_icon::TrayIcon,
    connect: MenuItem,
    disconnect: MenuItem,
    open: MenuItem,
    configure_telegram: MenuItem,
    test: MenuItem,
    exit: MenuItem,
    rx: std::sync::mpsc::Receiver<TrayAction>,
}

struct TeleRouteApp {
    ctx: AppContext,
    page: Page,
    last_refresh: Instant,
    diagnostics: Option<crate::diagnostics::DiagnosticReport>,
    diagnostics_rx: Option<std::sync::mpsc::Receiver<crate::diagnostics::DiagnosticReport>>,
    logs_cache: String,
    tray: Option<TrayState>,
    force_exit: bool,
}

impl TeleRouteApp {
    fn new(cc: &eframe::CreationContext<'_>, ctx: AppContext) -> anyhow::Result<Self> {
        let tray = build_tray(ctx.clone(), cc.egui_ctx.clone()).ok();

        Ok(Self {
            ctx,
            page: Page::Connection,
            last_refresh: Instant::now(),
            diagnostics: None,
            diagnostics_rx: None,
            logs_cache: String::new(),
            tray,
            force_exit: false,
        })
    }

    fn status_text(&self) -> &'static str {
        match self.ctx.status().status { ConnectionStatus::Connected => "READY", ConnectionStatus::Connecting => "CONNECTING", ConnectionStatus::Error => "ERROR", ConnectionStatus::Disconnected => "DISCONNECTED" }
    }

    fn mode_text(&self) -> String { format!("{:?}", self.ctx.config.read().routing.mode) }

    fn refresh_logs(&mut self) {
        let mut files: Vec<_> = std::fs::read_dir(crate::config::AppConfig::logs_dir()).ok().into_iter().flatten().flatten().collect();
        files.sort_by_key(|e| e.file_name());
        if let Some(path) = files.last().map(|e| e.path()) { if let Ok(text) = std::fs::read_to_string(path) { self.logs_cache = text.lines().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"); } }
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        let actions: Vec<TrayAction> = {
            let Some(tray) = self.tray.as_ref() else { return; };
            let mut actions = Vec::new();
            while let Ok(action) = tray.rx.try_recv() {
                actions.push(action);
            }
            actions
        };

        for action in actions {
            match action {
                TrayAction::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Test => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.page = Page::Diagnostics;
                    self.start_diagnostics();
                }
                TrayAction::ConfigureTelegram => {
                    let cfg = self.ctx.config.read().clone();
                    let host = match cfg.proxy.bind.as_str() {
                        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_owned(),
                        other => other.to_owned(),
                    };
                    let port = cfg.proxy.port;
                    self.ctx.spawn(async move {
                        let _ = tokio::task::spawn_blocking(move || {
                            #[cfg(windows)]
                            {
                                let _ = crate::platform::windows::open_telegram_socks_proxy(&host, port);
                            }
                        }).await;
                    });
                }
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
            if self.force_exit {
                self.ctx.disconnect();
                return;
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.ctx.config.read().autostart.minimize_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.ctx.disconnect();
                self.force_exit = true;
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
            egui::Panel::left("nav").min_size(178.0).show(ui, |ui| {
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
        let t = self.ctx.status();
        let s = self.ctx.snapshot();
        let cfg = self.ctx.config.read().clone();

        let route_transport = s.current_transport.to_string();
        let route_dc = s.current_dc
            .map(|v| v.to_string())
            .unwrap_or_else(|| t.dc.clone());

        ui.heading(
            egui::RichText::new("Connection")
                .size(24.0)
                .strong(),
        );
        ui.add_space(8.0);

        let (status_label, status_color) = match t.status {
            ConnectionStatus::Connected => ("READY", egui::Color32::from_rgb(75, 190, 120)),
            ConnectionStatus::Connecting => ("CONNECTING", egui::Color32::from_rgb(235, 180, 70)),
            ConnectionStatus::Error => ("ERROR", egui::Color32::from_rgb(225, 85, 85)),
            ConnectionStatus::Disconnected => ("DISCONNECTED", egui::Color32::from_rgb(145, 150, 160)),
        };

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("TeleRoute")
                                .size(13.0)
                                .weak(),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(status_label)
                                .size(28.0)
                                .strong()
                                .color(status_color),
                        );
                        ui.label(format!("Mode: {:?}", cfg.routing.mode));
                    });

                    ui.add_space(24.0);

                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Transport").weak());
                        ui.label(egui::RichText::new(&route_transport).strong());
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("DC").weak());
                        ui.label(egui::RichText::new(&route_dc).strong());
                    });

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let button_text =
                                if t.status == ConnectionStatus::Connected { "DISCONNECT" } else { "CONNECT" };
                            let button = egui::Button::new(
                                egui::RichText::new(button_text).strong().size(16.0),
                            );

                            if ui
                                .add_sized([190.0, 54.0], button)
                                .clicked()
                            {
                                if t.status == ConnectionStatus::Connected {
                                    self.ctx.disconnect();
                                } else {
                                    self.ctx.connect();
                                }
                            }
                        },
                    );
                });
            });

        ui.add_space(12.0);

        let cards = [
            ("Frontend", format!("{:?}", cfg.telegram.frontend)),
            ("MTProto", format!("{}:{}", cfg.telegram.mtproto_bind, cfg.telegram.mtproto_port)),
            ("TUN", t.tun.clone()),
            ("UDP", t.udp.clone()),
            ("Calls", t.calls.clone()),
            (
                "Ping",
                s.ping_ms
                    .map(|v| format!("{v} ms"))
                    .unwrap_or_else(|| "—".into()),
            ),
            ("Active", s.active_connections.to_string()),
            ("Upload", human_bytes(s.bytes_up)),
            ("Download", human_bytes(s.bytes_down)),
        ];

        let columns = ((ui.available_width() / 190.0).floor() as usize).clamp(1, 4);

        for row in cards.chunks(columns) {
            ui.columns(row.len(), |cols| {
                for (index, (title, value)) in row.iter().enumerate() {
                    responsive_card(&mut cols[index], title, value);
                }
            });
            ui.add_space(6.0);
        }

        if !t.error.is_empty() {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(225, 85, 85),
                        egui::RichText::new("Connection error").strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(&t.error);
                });
            ui.add_space(8.0);
        }

        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Telegram integration").strong());
            if cfg.telegram.configured_proxy.is_some() {
                ui.label(
                    egui::RichText::new("SOCKS5 configured")
                        .color(egui::Color32::from_rgb(75, 190, 120)),
                );
            } else {
                ui.label("Not registered yet");
            }

            if ui.button("Diagnostics").clicked() {
                self.page = Page::Diagnostics;
            }
        });

        ui.small("WSS connects lazily when Telegram creates its real MTProto connection. Calls require the separate UDP/TUN path.");
    }

    fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Diagnostics");
        if ui.button("Run full test").clicked() { self.start_diagnostics(); }
        if let Some(r) = &self.diagnostics {
            egui::Grid::new("diagnostics-grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in [("SOCKS5", &r.socks5), ("MTProto", &r.mtproto), ("DNS", &r.dns), ("Telegram TCP (direct)", &r.telegram_tcp), ("WebSocket (DC IP + SNI)", &r.websocket), ("TCP fallback", &r.tcp_fallback), ("UDP", &r.udp), ("TUN", &r.tun), ("Call transport", &r.call_transport)] {
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
        ui.collapsing("General", |ui| {
            ui.checkbox(&mut cfg.autostart.start_with_windows,"Start with Windows");
            ui.checkbox(&mut cfg.autostart.start_connected,"Start connected");
            ui.checkbox(&mut cfg.autostart.start_minimized,"Start minimized");
            ui.checkbox(&mut cfg.autostart.minimize_to_tray,"Minimize to tray");
            ui.separator();
            ui.checkbox(&mut cfg.telegram.auto_configure,"Automatically add proxy to Telegram once");
            ui.label(format!(
                "SOCKS5 registration: {}",
                cfg.telegram.configured_proxy.as_deref().unwrap_or("not registered")
            ));
            if ui.button("Mark current SOCKS5 as already configured").clicked() {
                let host = match cfg.proxy.bind.as_str() {
                    "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                    other => other,
                };
                cfg.telegram.configured_proxy = Some(format!("{}:{}", host, cfg.proxy.port));
            }
            if ui.button("Forget SOCKS5 Telegram registration").clicked() {
                cfg.telegram.configured_proxy = None;
            }
            ui.label(format!(
                "MTProto registration: {}",
                if cfg.telegram.configured_mtproto.is_some() { "registered" } else { "not registered" }
            ));
            if ui.button("Mark current MTProto as already configured").clicked() {
                cfg.telegram.configured_mtproto = Some(format!(
                    "{}:{}:{}",
                    cfg.mtproto.server.trim(),
                    cfg.mtproto.port,
                    cfg.mtproto.secret.trim()
                ));
            }
            if ui.button("Forget MTProto Telegram registration").clicked() {
                cfg.telegram.configured_mtproto = None;
            }
        });
        ui.collapsing("Telegram", |ui| {
            ui.label("Telegram-native frontend used by the WebSocket bridge:");

            ui.radio_value(
                &mut cfg.telegram.frontend,
                TelegramFrontend::MtprotoWs,
                "MTProto WebSocket (recommended)",
            );
            ui.radio_value(
                &mut cfg.telegram.frontend,
                TelegramFrontend::Socks5,
                "SOCKS5",
            );

            ui.checkbox(
                &mut cfg.telegram.auto_configure,
                "Automatically configure Telegram once",
            );

            match cfg.telegram.frontend {
                TelegramFrontend::MtprotoWs => {
                    ui.label("Calls/media use the Telegram MTProto media WebSocket path; TUN is optional.");
                    ui.horizontal(|ui| {
                        ui.label("Local port");
                        ui.add(egui::Slider::new(&mut cfg.telegram.mtproto_port, 1..=65535));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Secret");
                        ui.add_sized(
                            [ui.available_width().min(420.0), 28.0],
                            egui::TextEdit::singleline(&mut cfg.telegram.mtproto_secret),
                        );
                    });

                    if let Some(proxy) = &cfg.telegram.configured_mtproto {
                        ui.label(format!("Already configured: {proxy}"));
                    } else {
                        ui.label("Telegram MTProto proxy is not registered yet.");
                    }

                    if ui.button("Configure Telegram now").clicked() {
                        #[cfg(windows)]
                        {
                            let host = cfg.telegram.mtproto_bind.clone();
                            let tg_secret = format!("dd{}", cfg.telegram.mtproto_secret.trim());
                            if crate::platform::windows::open_telegram_mtproto_proxy(
                                &host,
                                cfg.telegram.mtproto_port,
                                &tg_secret,
                            ).is_ok() {
                                cfg.telegram.configured_mtproto = Some(format!(
                                    "{}:{}:{}",
                                    host,
                                    cfg.telegram.mtproto_port,
                                    cfg.telegram.mtproto_secret.trim()
                                ));
                            }
                        }
                    }

                    if ui.button("Forget Telegram MTProto registration").clicked() {
                        cfg.telegram.configured_mtproto = None;
                    }
                }

                TelegramFrontend::Socks5 => {
                    let host = match cfg.proxy.bind.as_str() {
                        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                        other => other,
                    };

                    ui.label("Generic local SOCKS5 frontend. Telegram media/calls are not guaranteed through SOCKS5.");
                    ui.label(format!("SOCKS5 endpoint: {host}:{}", cfg.proxy.port));

                    if let Some(proxy) = &cfg.telegram.configured_proxy {
                        ui.label(format!("Already configured: {proxy}"));
                    } else {
                        ui.label("Telegram SOCKS5 is not registered yet.");
                    }

                    if ui.button("Configure Telegram now").clicked() {
                        #[cfg(windows)]
                        {
                            if crate::platform::windows::open_telegram_socks_proxy(host, cfg.proxy.port).is_ok() {
                                cfg.telegram.configured_proxy = Some(format!("{host}:{}", cfg.proxy.port));
                            }
                        }
                    }

                    if ui.button("Forget Telegram SOCKS5 registration").clicked() {
                        cfg.telegram.configured_proxy = None;
                    }
                }
            }
        });
        ui.collapsing("Proxy", |ui| { ui.horizontal(|ui| { ui.label("Host"); ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.proxy.bind)); }); ui.add(egui::Slider::new(&mut cfg.proxy.port, 1..=65535).text("Port")); });
        ui.collapsing("MTProto", |ui| {
            ui.label("MTProto mode does not provide Telegram Calls.");
            ui.horizontal(|ui| {
                ui.label("Server");
                ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.mtproto.server));
            });
            ui.add(egui::Slider::new(&mut cfg.mtproto.port, 1..=65535).text("Port"));
            ui.horizontal(|ui| {
                ui.label("Secret");
                ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.mtproto.secret));
            });
            ui.label("Example secret: 32 hexadecimal characters.");
        });
        ui.collapsing("WebSocket", |ui| { ui.add(egui::Slider::new(&mut cfg.timeouts.connect_ms, 500..=15000).text("Connect timeout ms")); ui.add(egui::Slider::new(&mut cfg.timeouts.reconnect_ms, 100..=10000).text("Reconnect delay ms")); ui.label("Endpoint templates are configured in config.toml."); });
        ui.collapsing("Routing", |ui| { for mode in [Mode::Proxy,Mode::Calls,Mode::Full,Mode::Mtproto] { ui.radio_value(&mut cfg.routing.mode, mode, format!("{mode:?}")); } ui.checkbox(&mut cfg.routing.telegram_only,"Telegram-only UDP routing"); ui.checkbox(&mut cfg.routing.direct_udp_fallback,"Direct UDP fallback"); ui.checkbox(&mut cfg.routing.relay_udp_fallback,"QUIC relay fallback"); ui.checkbox(&mut cfg.routing.route_all_traffic,"Route all traffic (requires a future full TCP userspace stack; disabled now)"); });
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
fn build_tray(ctx: AppContext, egui_ctx: egui::Context) -> anyhow::Result<TrayState> {
    let menu = Menu::new();
    let connect = MenuItem::new("Connect", true, None);
    let disconnect = MenuItem::new("Disconnect", true, None);
    let open = MenuItem::new("Open", true, None);
    let configure_telegram = MenuItem::new("Configure Telegram", true, None);
    let test = MenuItem::new("Test", true, None);
    let exit = MenuItem::new("Exit", true, None);
    menu.append_items(&[&connect, &disconnect, &open, &configure_telegram, &test, &exit])?;

    let (tx, rx) = std::sync::mpsc::channel::<TrayAction>();
    let connect_id = connect.id().clone();
    let disconnect_id = disconnect.id().clone();
    let open_id = open.id().clone();
    let configure_telegram_id = configure_telegram.id().clone();
    let test_id = test.id().clone();
    let exit_id = exit.id().clone();

    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
        if event.id() == &connect_id {
            ctx.connect();
        } else if event.id() == &disconnect_id {
            ctx.disconnect();
        } else if event.id() == &open_id {
            let _ = tx.send(TrayAction::Open);
            egui_ctx.request_repaint();
        } else if event.id() == &configure_telegram_id {
            let _ = tx.send(TrayAction::ConfigureTelegram);
            egui_ctx.request_repaint();
        } else if event.id() == &test_id {
            let _ = tx.send(TrayAction::Test);
            egui_ctx.request_repaint();
        } else if event.id() == &exit_id {
            ctx.disconnect();
            std::process::exit(0);
        }
    }));

    let rgba: Vec<u8> = std::iter::repeat([0x20u8, 0x95u8, 0xffu8, 0xffu8])
        .take(16 * 16)
        .flatten()
        .collect();
    let icon = Icon::from_rgba(rgba, 16, 16)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("TeleRoute")
        .with_icon(icon)
        .build()?;

    Ok(TrayState {
        _tray: tray,
        connect,
        disconnect,
        open,
        configure_telegram,
        test,
        exit,
        rx,
    })
}
