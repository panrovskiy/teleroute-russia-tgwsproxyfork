use crate::{app::{AppContext, ConnectionStatus}, config::{Mode, TelegramFrontend}};
use eframe::egui;
use rand::{rng, RngCore};
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
    diagnostics_running: bool,
    egui_ctx: egui::Context,
    logs_cache: String,
    tray: Option<TrayState>,
    force_exit: bool,
    last_page: Page,
    page_started: Instant,
}

impl TeleRouteApp {
    fn new(cc: &eframe::CreationContext<'_>, ctx: AppContext) -> anyhow::Result<Self> {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(12, 16, 25);
        visuals.window_fill = egui::Color32::from_rgb(17, 23, 35);
        visuals.extreme_bg_color = egui::Color32::from_rgb(8, 11, 18);
        visuals.faint_bg_color = egui::Color32::from_rgb(25, 34, 49);
        visuals.code_bg_color = egui::Color32::from_rgb(10, 14, 22);
        visuals.hyperlink_color = egui::Color32::from_rgb(100, 183, 255);
        visuals.selection.bg_fill = egui::Color32::from_rgb(36, 106, 181);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(20, 28, 42);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(27, 38, 55);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 58, 83);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(37, 116, 202);
        visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(206, 218, 234);
        cc.egui_ctx.set_visuals(visuals);

        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        style.spacing.interact_size.y = 34.0;
        cc.egui_ctx.set_style(style);

        let now = Instant::now();
        let tray = build_tray(ctx.clone(), cc.egui_ctx.clone()).ok();

        Ok(Self {
            ctx,
            page: Page::Connection,
            last_refresh: now,
            diagnostics: None,
            diagnostics_rx: None,
            diagnostics_running: false,
            egui_ctx: cc.egui_ctx.clone(),
            logs_cache: String::new(),
            tray,
            force_exit: false,
            last_page: Page::Connection,
            page_started: now,
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
        if self.diagnostics_running {
            return;
        }

        self.diagnostics_running = true;
        self.diagnostics = Some(crate::diagnostics::DiagnosticReport {
            socks5: "RUNNING".into(),
            mtproto: "RUNNING".into(),
            dns: "RUNNING".into(),
            telegram_tcp: "RUNNING".into(),
            websocket: "RUNNING".into(),
            media_websocket: "RUNNING".into(),
            tcp_fallback: "RUNNING".into(),
            udp: "RUNNING".into(),
            tun: "RUNNING".into(),
            call_transport: "RUNNING".into(),
        });

        let app_ctx = self.ctx.clone();
        let task_ctx = app_ctx.clone();
        let repaint_ctx = self.egui_ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        app_ctx.spawn(async move {
            let result = task_ctx.run_diagnostics().await;
            let _ = tx.send(result);
            repaint_ctx.request_repaint();
        });
        self.diagnostics_rx = Some(rx);
        self.egui_ctx.request_repaint();
    }
}

impl eframe::App for TeleRouteApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_tray(&ctx);
        let diagnostics_result = self.diagnostics_rx.as_ref().map(|rx| rx.try_recv());
        if let Some(result) = diagnostics_result {
            match result {
                Ok(report) => {
                    self.diagnostics = Some(report);
                    self.diagnostics_rx = None;
                    self.diagnostics_running = false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if self.diagnostics_running {
                        ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.diagnostics = Some(crate::diagnostics::DiagnosticReport {
                        socks5: "DIAGNOSTIC TASK FAILED".into(),
                        mtproto: "DIAGNOSTIC TASK FAILED".into(),
                        dns: "DIAGNOSTIC TASK FAILED".into(),
                        telegram_tcp: "DIAGNOSTIC TASK FAILED".into(),
                        websocket: "DIAGNOSTIC TASK FAILED".into(),
                        media_websocket: "DIAGNOSTIC TASK FAILED".into(),
                        tcp_fallback: "DIAGNOSTIC TASK FAILED".into(),
                        udp: "DIAGNOSTIC TASK FAILED".into(),
                        tun: "DIAGNOSTIC TASK FAILED".into(),
                        call_transport: "DIAGNOSTIC TASK FAILED".into(),
                    });
                    self.diagnostics_rx = None;
                    self.diagnostics_running = false;
                }
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

        if self.page != self.last_page {
            self.last_page = self.page;
            self.page_started = Instant::now();
        }
        let page_progress = (self.page_started.elapsed().as_secs_f32() / 0.18).clamp(0.0, 1.0);
        if page_progress < 1.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        let narrow = ui.available_width() < 820.0;

        let header_status = self.ctx.status();
        let (header_text, header_color) = match header_status.status {
            ConnectionStatus::Connected => ("LOCAL LISTENER READY", egui::Color32::from_rgb(82, 205, 145)),
            ConnectionStatus::Connecting => ("STARTING", egui::Color32::from_rgb(242, 189, 91)),
            ConnectionStatus::Error => ("NEEDS ATTENTION", egui::Color32::from_rgb(238, 103, 110)),
            ConnectionStatus::Disconnected => ("OFFLINE", egui::Color32::from_rgb(155, 167, 184)),
        };

        egui::Panel::top("top").show(ui, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(15, 21, 32))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(36, 48, 67)))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(39, 117, 202))
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("TR").size(20.0).strong().color(egui::Color32::WHITE));
                            });
                        ui.vertical(|ui| {
                            ui.heading(egui::RichText::new("TeleRoute").size(22.0).strong());
                            ui.label(egui::RichText::new("Telegram connection manager").size(12.0).weak());
                        });
                        ui.add_space(12.0);
                        ui.separator();
                        ui.label(egui::RichText::new(header_text).strong().color(header_color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if header_status.status == ConnectionStatus::Connected {
                                if ui.add_sized([128.0, 38.0], egui::Button::new("Disconnect").fill(egui::Color32::from_rgb(134, 53, 68))).clicked() {
                                    self.ctx.disconnect();
                                }
                            } else if header_status.status != ConnectionStatus::Connecting {
                                if ui.add_sized([128.0, 38.0], egui::Button::new("Connect").fill(egui::Color32::from_rgb(35, 126, 92))).clicked() {
                                    self.ctx.connect();
                                }
                            } else {
                                ui.add(egui::Spinner::new());
                            }
                        });
                    });
                });
        });

        if !narrow {
            egui::Panel::left("nav").min_size(196.0).show(ui, |ui| {
                self.navigation(ui, false);
            });
        } else {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(14, 20, 30))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal().id_salt("compact-navigation").show(ui, |ui| {
                        self.navigation(ui, true);
                    });
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(11, 15, 23))
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.add_space((1.0 - page_progress) * 8.0);
                    match self.page {
                        Page::Connection => self.connection_page(ui),
                        Page::Diagnostics => self.diagnostics_page(ui),
                        Page::Statistics => self.statistics_page(ui),
                        Page::Logs => self.logs_page(ui),
                        Page::Settings => self.settings_page(ui),
                    }
                });
        });
    }
}

impl TeleRouteApp {
    fn navigation(&mut self, ui: &mut egui::Ui, compact: bool) {
        let items = [
            (Page::Connection, "Overview"),
            (Page::Diagnostics, "Diagnostics"),
            (Page::Statistics, "Statistics"),
            (Page::Logs, "Logs"),
            (Page::Settings, "Settings"),
        ];

        if compact {
            ui.horizontal_wrapped(|ui| {
                for (index, (page, name)) in items.iter().enumerate() {
                    let label = format!("{:02}  {}", index + 1, name);
                    if ui.add_sized([132.0, 36.0], egui::Button::selectable(self.page == *page, label)).clicked() {
                        self.page = *page;
                    }
                }
            });
            return;
        }

        egui::Frame::new()
            .fill(egui::Color32::from_rgb(14, 20, 30))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("WORKSPACE").size(11.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));
                ui.add_space(8.0);

                for (index, (page, name)) in items.iter().enumerate() {
                    let label = format!("{:02}    {}", index + 1, name);
                    if ui.add_sized([ui.available_width(), 42.0], egui::Button::selectable(self.page == *page, label)).clicked() {
                        self.page = *page;
                    }
                }

                ui.add_space(18.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(egui::RichText::new("QUICK STATUS").size(11.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));

                let status = self.ctx.status();
                let status_text = match status.status {
                    ConnectionStatus::Connected => "Listener running",
                    ConnectionStatus::Connecting => "Starting services",
                    ConnectionStatus::Error => "Startup error",
                    ConnectionStatus::Disconnected => "Disconnected",
                };
                let status_color = match status.status {
                    ConnectionStatus::Connected => egui::Color32::from_rgb(82, 205, 145),
                    ConnectionStatus::Connecting => egui::Color32::from_rgb(242, 189, 91),
                    ConnectionStatus::Error => egui::Color32::from_rgb(238, 103, 110),
                    ConnectionStatus::Disconnected => egui::Color32::from_rgb(155, 167, 184),
                };

                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(20, 29, 43))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(status_text).strong().color(status_color));
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(format!("Mode: {:?}", self.ctx.config.read().routing.mode)).size(12.0).weak());
                        ui.label(egui::RichText::new(format!("Frontend: {:?}", self.ctx.config.read().telegram.frontend)).size(12.0).weak());
                    });

                ui.add_space(10.0);
                ui.label(egui::RichText::new("A running listener does not guarantee that Telegram's upstream route is healthy.").size(11.0).weak());
            });
    }

    fn connection_page(&mut self, ui: &mut egui::Ui) {
        let telemetry = self.ctx.status();
        let stats = self.ctx.snapshot();
        let config = self.ctx.config.read().clone();
        let has_payload = stats.bytes_up > 0 || stats.bytes_down > 0;

        let transport_text = if stats.current_transport != "—" {
            stats.current_transport.to_string()
        } else if telemetry.status == ConnectionStatus::Connected {
            "Waiting for Telegram traffic".into()
        } else {
            "—".into()
        };
        let dc_text = stats.current_dc
            .map(|value| format!("DC {value}"))
            .unwrap_or_else(|| telemetry.dc.clone());

        let (status_label, status_color, status_detail) = match telemetry.status {
            ConnectionStatus::Connected if has_payload => (
                "TRAFFIC ACTIVE",
                egui::Color32::from_rgb(82, 205, 145),
                "Telegram data is moving through the local proxy.",
            ),
            ConnectionStatus::Connected => (
                "LOCAL LISTENER READY",
                egui::Color32::from_rgb(242, 189, 91),
                "The local port is open. The upstream route is checked separately.",
            ),
            ConnectionStatus::Connecting => (
                "STARTING",
                egui::Color32::from_rgb(242, 189, 91),
                "Starting the local Telegram frontend.",
            ),
            ConnectionStatus::Error => (
                "NEEDS ATTENTION",
                egui::Color32::from_rgb(238, 103, 110),
                "The frontend could not start. See the error details below.",
            ),
            ConnectionStatus::Disconnected => (
                "OFFLINE",
                egui::Color32::from_rgb(155, 167, 184),
                "Connect to start the local Telegram frontend.",
            ),
        };

        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.heading(egui::RichText::new("Overview").size(27.0).strong());
                ui.label(egui::RichText::new("Connection health, transport and live traffic").weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(format!("MODE  {:?}", config.routing.mode)).strong().color(egui::Color32::from_rgb(118, 184, 255)));
            });
        });
        ui.add_space(10.0);

        egui::Frame::new()
            .fill(egui::Color32::from_rgb(19, 29, 44))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 62, 86)))
            .inner_margin(egui::Margin::same(20))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("TELEGRAM ROUTER").size(11.0).strong().color(egui::Color32::from_rgb(129, 156, 188)));
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(status_label).size(24.0).strong().color(status_color));
                        ui.add_space(3.0);
                        ui.label(status_detail);
                        ui.add_space(12.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(format!("Frontend  {:?}", config.telegram.frontend)).strong());
                            ui.separator();
                            ui.label(egui::RichText::new(format!("Transport  {transport_text}")).strong());
                            ui.separator();
                            ui.label(egui::RichText::new(format!("Route  {dc_text}")).strong());
                        });
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (label, fill) = if telemetry.status == ConnectionStatus::Connected {
                            ("Disconnect", egui::Color32::from_rgb(134, 53, 68))
                        } else {
                            ("Connect", egui::Color32::from_rgb(35, 126, 92))
                        };
                        let button = egui::Button::new(egui::RichText::new(label).strong().size(16.0)).fill(fill);
                        if ui.add_sized([150.0, 48.0], button).clicked() {
                            if telemetry.status == ConnectionStatus::Connected {
                                self.ctx.disconnect();
                            } else {
                                self.ctx.connect();
                            }
                        }
                    });
                });
            });

        ui.add_space(16.0);
        ui.label(egui::RichText::new("LIVE METRICS").size(12.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));
        ui.add_space(4.0);

        let values = [
            ("WSS latency", stats.ping_ms.map(|value| format!("{value} ms")).unwrap_or_else(|| if telemetry.status == ConnectionStatus::Connected { "Checking…".into() } else { "—".into() })),
            ("Calls / media", telemetry.calls.clone()),
            ("TUN adapter", telemetry.tun.clone()),
            ("UDP transport", telemetry.udp.clone()),
            ("Active sessions", stats.active_connections.to_string()),
            ("Upload", human_bytes(stats.bytes_up)),
            ("Download", human_bytes(stats.bytes_down)),
            ("MTProto endpoint", format!("{}:{}", config.telegram.mtproto_bind, config.telegram.mtproto_port)),
        ];
        let columns = ((ui.available_width() / 230.0).floor() as usize).clamp(1, 4);
        for row in values.chunks(columns) {
            ui.columns(row.len(), |cols| {
                for (index, (title, value)) in row.iter().enumerate() {
                    responsive_card(&mut cols[index], title, value);
                }
            });
        }

        ui.add_space(12.0);
        if !telemetry.error.is_empty() {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(43, 24, 31))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(119, 54, 66)))
                .inner_margin(egui::Margin::same(13))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Connection warning").strong().color(egui::Color32::from_rgb(238, 123, 130)));
                    ui.label(&telemetry.error);
                });
        }

        ui.add_space(8.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(16, 23, 34))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("NEXT STEP").strong().color(egui::Color32::from_rgb(118, 184, 255)));
                    ui.label("Open Diagnostics to test DNS, the standard WSS route and the media WSS route.");
                    if ui.button("Open diagnostics").clicked() {
                        self.page = Page::Diagnostics;
                    }
                });
                ui.add_space(4.0);
                ui.small("WSS latency measures the time to establish a WebSocket route, not an ICMP ping. Media WSS availability does not by itself verify an end-to-end Telegram call.");
            });
    }
    fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new("Diagnostics").size(27.0).strong());
        ui.label(egui::RichText::new("Test the local listener and upstream route independently.").weak());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if !self.diagnostics_running && ui.button("Run full test").clicked() {
                self.start_diagnostics();
            }
            if self.diagnostics_running {
                ui.add(egui::Spinner::new());
                ui.label("Checking DNS, Telegram routes and WebSocket fallbacks…");
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        if let Some(r) = &self.diagnostics {
            egui::Grid::new("diagnostics-grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in [("SOCKS5", &r.socks5), ("MTProto", &r.mtproto), ("DNS", &r.dns), ("Telegram TCP (direct)", &r.telegram_tcp), ("WebSocket (Flowseal DC route)", &r.websocket), ("Media WebSocket", &r.media_websocket), ("TCP fallback", &r.tcp_fallback), ("UDP", &r.udp), ("TUN", &r.tun), ("Call transport", &r.call_transport)] {
                    ui.strong(k);
                    let value = v.as_str();
                    let lower = value.to_ascii_lowercase();
                    let color = if lower.contains("ok") || lower.contains("listening") || lower.contains("available") || lower == "active" {
                        egui::Color32::from_rgb(83, 190, 130)
                    } else if lower.contains("failed") || lower.contains("blocked") || lower.contains("unavailable") {
                        egui::Color32::from_rgb(225, 95, 95)
                    } else if lower.contains("stopped") || lower.contains("not running") || lower.contains("not listening") {
                        egui::Color32::from_rgb(150, 158, 171)
                    } else {
                        egui::Color32::from_rgb(230, 184, 92)
                    };
                    ui.label(egui::RichText::new(value).color(color));
                    ui.end_row();
                }
            });
        }
        else { ui.label("No diagnostic run yet."); }
    }

    fn statistics_page(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new("Statistics").size(27.0).strong());
        ui.label(egui::RichText::new("Traffic totals, transport reliability and call-path counters.").weak());
        ui.add_space(8.0);
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
                cfg.telegram.configured_proxy.as_deref().and_then(|value| value.strip_prefix("v3:")).unwrap_or("not registered")
            ));
            if ui.button("Mark current SOCKS5 as already configured").clicked() {
                let host = match cfg.proxy.bind.as_str() {
                    "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                    other => other,
                };
                cfg.telegram.configured_proxy = Some(format!("v3:{}:{}", host, cfg.proxy.port));
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
                    "v3:{}:{}:{}",
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
                        let response = ui.add_sized(
                            [ui.available_width().min(420.0), 28.0],
                            egui::TextEdit::singleline(&mut cfg.telegram.mtproto_secret),
                        );
                        if response.changed() {
                            cfg.telegram.configured_mtproto = None;
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Randomize secret").clicked() {
                            cfg.telegram.mtproto_secret = generate_random_secret();
                            cfg.telegram.configured_mtproto = None;
                        }

                        if ui.button("Randomize + configure Telegram").clicked() {
                            cfg.telegram.mtproto_secret = generate_random_secret();
                            cfg.telegram.configured_mtproto = None;

                            #[cfg(windows)]
                            {
                                let host = cfg.telegram.mtproto_bind.clone();
                                let secret = format!("dd{}", cfg.telegram.mtproto_secret.trim());
                                if crate::platform::windows::open_telegram_mtproto_proxy(
                                    &host,
                                    cfg.telegram.mtproto_port,
                                    &secret,
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

fn generate_random_secret() -> String {
    let mut bytes = [0u8; 16];
    rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn responsive_card(ui: &mut egui::Ui, title: &str, value: &str) {
    let width = ui.available_width().max(1.0);
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(19, 27, 40))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 54, 74)))
        .inner_margin(egui::Margin::same(13))
        .show(ui, |ui| {
            ui.set_min_size([width, 78.0].into());
            ui.label(egui::RichText::new(title).size(11.0).strong().color(egui::Color32::from_rgb(130, 151, 178)));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(value).strong().size(15.0));
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
