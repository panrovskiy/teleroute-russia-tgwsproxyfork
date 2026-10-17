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
    settings_notice: String,
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

        let mut style = (*cc.egui_ctx.style_of(egui::Theme::Dark)).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        style.spacing.interact_size.y = 34.0;
        cc.egui_ctx.set_style_of(egui::Theme::Dark, style);

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
            settings_notice: String::new(),
        })
    }

    fn status_text(&self) -> &'static str {
        match self.ctx.status().status { ConnectionStatus::Connected => "ПРОКСИ ГОТОВ", ConnectionStatus::Connecting => "ПОДКЛЮЧЕНИЕ", ConnectionStatus::Stopping => "ОСТАНОВКА", ConnectionStatus::Error => "ОШИБКА", ConnectionStatus::Disconnected => "ОТКЛЮЧЕНО" }
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
            ConnectionStatus::Connected => ("ЛОКАЛЬНЫЙ ПРОКСИ ГОТОВ", egui::Color32::from_rgb(82, 205, 145)),
            ConnectionStatus::Connecting => ("ЗАПУСК", egui::Color32::from_rgb(242, 189, 91)),
            ConnectionStatus::Stopping => ("ОСТАНОВКА", egui::Color32::from_rgb(242, 189, 91)),
            ConnectionStatus::Error => ("ТРЕБУЕТ ВНИМАНИЯ", egui::Color32::from_rgb(238, 103, 110)),
            ConnectionStatus::Disconnected => ("НЕ ПОДКЛЮЧЕНО", egui::Color32::from_rgb(155, 167, 184)),
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
                            ui.label(egui::RichText::new("Управление подключением к Telegram").size(12.0).weak());
                        });
                        ui.add_space(12.0);
                        ui.separator();
                        ui.label(egui::RichText::new(header_text).strong().color(header_color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if header_status.status == ConnectionStatus::Connected {
                                if ui.add_sized([128.0, 38.0], egui::Button::new("Отключить").fill(egui::Color32::from_rgb(134, 53, 68))).clicked() {
                                    self.ctx.disconnect();
                                }
                            } else if header_status.status != ConnectionStatus::Connecting
                                && header_status.status != ConnectionStatus::Stopping {
                                if ui.add_sized([128.0, 38.0], egui::Button::new("Подключить").fill(egui::Color32::from_rgb(35, 126, 92))).clicked() {
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
            (Page::Connection, "Обзор"),
            (Page::Diagnostics, "Диагностика"),
            (Page::Statistics, "Статистика"),
            (Page::Logs, "Журнал"),
            (Page::Settings, "Настройки"),
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
                ui.label(egui::RichText::new("РАБОЧЕЕ ПРОСТРАНСТВО").size(11.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));
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
                ui.label(egui::RichText::new("СОСТОЯНИЕ").size(11.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));

                let status = self.ctx.status();
                let status_text = match status.status {
                    ConnectionStatus::Connected => "Локальный прокси запущен",
                    ConnectionStatus::Connecting => "Запуск служб",
                    ConnectionStatus::Stopping => "Остановка слушателей",
                    ConnectionStatus::Error => "Ошибка запуска",
                    ConnectionStatus::Disconnected => "Отключено",
                };
                let status_color = match status.status {
                    ConnectionStatus::Connected => egui::Color32::from_rgb(82, 205, 145),
                    ConnectionStatus::Connecting | ConnectionStatus::Stopping => egui::Color32::from_rgb(242, 189, 91),
                    ConnectionStatus::Error => egui::Color32::from_rgb(238, 103, 110),
                    ConnectionStatus::Disconnected => egui::Color32::from_rgb(155, 167, 184),
                };

                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(20, 29, 43))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(status_text).strong().color(status_color));
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(format!("Режим: {}", mode_label(self.ctx.config.read().routing.mode))).size(12.0).weak());
                        ui.label(egui::RichText::new(format!("Прокси: {}", frontend_label(self.ctx.config.read().telegram.frontend))).size(12.0).weak());
                    });

                ui.add_space(10.0);
                ui.label(egui::RichText::new("Запущенный локальный порт не гарантирует, что соединение с серверами Telegram работает.").size(11.0).weak());
            });
    }

    fn connection_page(&mut self, ui: &mut egui::Ui) {
        let telemetry = self.ctx.status();
        let stats = self.ctx.snapshot();
        let config = self.ctx.config.read().clone();
        let active_sessions = stats.active_connections;
        let has_live_session = active_sessions > 0;

        let transport_text = if stats.current_transport != "—" {
            stats.current_transport.to_string()
        } else if telemetry.status == ConnectionStatus::Connected {
            "Ожидание трафика Telegram".into()
        } else {
            "—".into()
        };
        let dc_text = stats.current_dc
            .map(|value| format!("DC {value}"))
            .unwrap_or_else(|| telemetry.dc.clone());

        let (status_label, status_color, status_detail) = match telemetry.status {
            ConnectionStatus::Connected if has_live_session => (
                "СЕАНС TELEGRAM АКТИВЕН",
                egui::Color32::from_rgb(82, 205, 145),
                "Клиент успешно выполнил рукопожатие с локальным прокси.",
            ),
            ConnectionStatus::Connected => (
                "LOCAL LISTENER READY",
                egui::Color32::from_rgb(242, 189, 91),
                "Локальный порт открыт, но активного сеанса Telegram нет. Возможно, в Telegram не выбран сохранённый прокси или используется старая запись с другим секретом.",
            ),
            ConnectionStatus::Connecting => (
                "STARTING",
                egui::Color32::from_rgb(242, 189, 91),
                "Запуск локального прокси Telegram.",
            ),
            ConnectionStatus::Stopping => (
                "STOPPING",
                egui::Color32::from_rgb(242, 189, 91),
                "Закрытие локальных слушателей и сеансов перед повторным запуском.",
            ),
            ConnectionStatus::Error => (
                "NEEDS ATTENTION",
                egui::Color32::from_rgb(238, 103, 110),
                "Локальный прокси остановился или не смог запуститься. Подробности приведены ниже.",
            ),
            ConnectionStatus::Disconnected => (
                "OFFLINE",
                egui::Color32::from_rgb(155, 167, 184),
                "Нажмите «Подключить», чтобы запустить локальный прокси Telegram.",
            ),
        };

        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.heading(egui::RichText::new("Обзор").size(27.0).strong());
                ui.label(egui::RichText::new("Состояние соединения, транспорт и текущий трафик").weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(format!("РЕЖИМ  {}", mode_label(config.routing.mode))).strong().color(egui::Color32::from_rgb(118, 184, 255)));
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
                        ui.label(egui::RichText::new("МАРШРУТИЗАТОР TELEGRAM").size(11.0).strong().color(egui::Color32::from_rgb(129, 156, 188)));
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(status_label).size(24.0).strong().color(status_color));
                        ui.add_space(3.0);
                        ui.label(status_detail);
                        ui.add_space(12.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(format!("Прокси  {}", frontend_label(config.telegram.frontend))).strong());
                            ui.separator();
                            ui.label(egui::RichText::new(format!("Transport  {transport_text}")).strong());
                            ui.separator();
                            ui.label(egui::RichText::new(format!("Route  {dc_text}")).strong());
                        });
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (label, fill) = match telemetry.status {
                            ConnectionStatus::Connected => ("Отключить", egui::Color32::from_rgb(134, 53, 68)),
                            ConnectionStatus::Connecting => ("Запуск…", egui::Color32::from_rgb(72, 86, 105)),
                            ConnectionStatus::Stopping => ("Остановка…", egui::Color32::from_rgb(72, 86, 105)),
                            ConnectionStatus::Error | ConnectionStatus::Disconnected => ("Подключить", egui::Color32::from_rgb(35, 126, 92)),
                        };
                        let button = egui::Button::new(egui::RichText::new(label).strong().size(16.0)).fill(fill);
                        if ui.add_sized([150.0, 48.0], button).clicked() {
                            match telemetry.status {
                                ConnectionStatus::Connected => self.ctx.disconnect(),
                                ConnectionStatus::Disconnected | ConnectionStatus::Error => self.ctx.connect(),
                                ConnectionStatus::Connecting | ConnectionStatus::Stopping => {}
                            }
                        }
                    });
                });
            });

        ui.add_space(16.0);
        ui.label(egui::RichText::new("ТЕКУЩИЕ ПОКАЗАТЕЛИ").size(12.0).strong().color(egui::Color32::from_rgb(122, 145, 174)));
        ui.add_space(4.0);

        let values = [
            ("Задержка WSS", stats.ping_ms.map(|value| format!("{value} ms")).unwrap_or_else(|| if telemetry.status == ConnectionStatus::Connected { "Проверка…".into() } else { "—".into() })),
            ("Звонки и медиа", localize_status(&telemetry.calls)),
            ("Адаптер TUN", localize_status(&telemetry.tun)),
            ("Транспорт UDP", localize_status(&telemetry.udp)),
            ("Клиент Telegram", localize_status(&telemetry.telegram_client)),
            ("Активные сеансы", stats.active_connections.to_string()),
            ("Отправлено", human_bytes(stats.bytes_up)),
            ("Получено", human_bytes(stats.bytes_down)),
            ("Адрес MTProto", format!("{}:{}", config.telegram.mtproto_bind, config.telegram.mtproto_port)),
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
                    ui.label(egui::RichText::new("Предупреждение о соединении").strong().color(egui::Color32::from_rgb(238, 123, 130)));
                    ui.label(localize_error(&telemetry.error));
                });
        }

        ui.add_space(8.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(16, 23, 34))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("СЛЕДУЮЩИЙ ШАГ").strong().color(egui::Color32::from_rgb(118, 184, 255)));
                    ui.label("Откройте диагностику, чтобы проверить DNS, основной маршрут WSS и маршрут для медиа.");
                    if ui.button("Открыть диагностику").clicked() {
                        self.page = Page::Diagnostics;
                    }
                });
                if telemetry.status == ConnectionStatus::Connected && active_sessions == 0 {
                    ui.add_space(8.0);
                    ui.small("Важно: TeleRoute не может узнать, какой прокси выбран внутри Telegram. Чтобы не создавать дубликаты, приложение не открывает ссылку tg://proxy при каждом запуске. Откройте настройки соединения Telegram и выберите сохранённый прокси TeleRoute.");
                }
                ui.add_space(4.0);
                ui.small("Задержка WSS — это время установления WebSocket-соединения, а не ICMP-пинг. Проверка Media WSS подтверждает только успешное рукопожатие, но не гарантирует загрузку файлов или работу звонков.");
            });
    }
    fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        page_heading(
            ui,
            "Диагностика",
            "Отдельно проверьте локальный прокси, маршруты WebSocket Telegram и маршрут медиа.",
        );
        ui.add_space(10.0);

        egui::Frame::new()
            .fill(egui::Color32::from_rgb(17, 25, 38))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(41, 57, 79)))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if !self.diagnostics_running
                        && ui.add_sized(
                            [160.0, 40.0],
                            egui::Button::new(egui::RichText::new("Запустить полную проверку").strong())
                                .fill(egui::Color32::from_rgb(34, 110, 186)),
                        ).clicked()
                    {
                        self.start_diagnostics();
                    }

                    if self.diagnostics_running {
                        ui.add(egui::Spinner::new());
                        ui.label("Проверка DNS, адресов WSS и резервного маршрута медиа…");
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    } else {
                        ui.label(egui::RichText::new("Проверка выполняется в фоне; интерфейс продолжает работать.").weak());
                    }
                });
            });

        ui.add_space(10.0);
        if let Some(report) = &self.diagnostics {
            let items = [
                ("Слушатель SOCKS5", report.socks5.as_str()),
                ("Слушатель MTProto", report.mtproto.as_str()),
                ("Разрешение DNS", report.dns.as_str()),
                ("Прямой TCP до Telegram", report.telegram_tcp.as_str()),
                ("Маршрут WebSocket", report.websocket.as_str()),
                ("WebSocket для медиа", report.media_websocket.as_str()),
                ("Резервный TCP", report.tcp_fallback.as_str()),
                ("Сокет UDP", report.udp.as_str()),
                ("Адаптер TUN", report.tun.as_str()),
                ("Транспорт звонков", report.call_transport.as_str()),
            ];

            let columns = ((ui.available_width() / 240.0).floor() as usize).clamp(1, 3);
            for row in items.chunks(columns) {
                ui.columns(row.len(), |cols| {
                    for (index, (title, value)) in row.iter().enumerate() {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(19, 27, 40))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 54, 74)))
                            .inner_margin(egui::Margin::same(13))
                            .show(&mut cols[index], |ui| {
                                ui.label(egui::RichText::new(*title).size(11.0).strong().color(egui::Color32::from_rgb(130, 151, 178)));
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(localize_status(value)).strong().color(diagnostic_color(value)));
                            });
                    }
                });
                ui.add_space(7.0);
            }

            ui.add_space(4.0);
            ui.small("Успешное рукопожатие WSS подтверждает доступность маршрута, но не доказывает, что фотография полностью загрузится или голосовой/видеозвонок будет работать.");
        } else {
            empty_state(
                ui,
                "Проверка ещё не запускалась",
                "Запустите проверку, чтобы узнать, какие маршруты доступны и где возникает ошибка.",
            );
        }
    }

    fn statistics_page(&mut self, ui: &mut egui::Ui) {
        page_heading(
            ui,
            "Статистика",
            "Счётчики трафика и качество соединения. Значения обновляются по мере передачи данных.",
        );
        ui.add_space(10.0);
        let stats = self.ctx.snapshot();
        let values = [
            ("Задержка маршрута WSS", stats.ping_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into())),
            ("Всего соединений", stats.connections.to_string()),
            ("Активные соединения", stats.active_connections.to_string()),
            ("Отправлено", human_bytes(stats.bytes_up)),
            ("Получено", human_bytes(stats.bytes_down)),
            ("Отправлено пакетов", stats.packets_up.to_string()),
            ("Получено пакетов", stats.packets_down.to_string()),
            ("Успешные подключения WSS", format!("{:.1}%", stats.ws_success_rate)),
            ("Переходы на резервный TCP", stats.tcp_fallbacks.to_string()),
            ("Сеансы UDP", stats.udp_sessions.to_string()),
            ("Заблокировано UDP", stats.udp_blocked.to_string()),
            ("Отправлено в звонках", human_bytes(stats.call_bytes_up)),
            ("Получено в звонках", human_bytes(stats.call_bytes_down)),
            ("Джиттер звонков", stats.call_jitter_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into())),
        ];
        let columns = ((ui.available_width() / 230.0).floor() as usize).clamp(1, 4);
        for row in values.chunks(columns) {
            ui.columns(row.len(), |cols| {
                for (index, (title, value)) in row.iter().enumerate() {
                    responsive_card(&mut cols[index], title, value);
                }
            });
            ui.add_space(7.0);
        }

        ui.add_space(8.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(16, 23, 34))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("О задержке").strong().color(egui::Color32::from_rgb(118, 184, 255)));
                ui.label("Задержка показывает время установления маршрута WebSocket. Это не ICMP-пинг и не полное время ответа Telegram.");
            });
    }

    fn logs_page(&mut self, ui: &mut egui::Ui) {
        page_heading(
            ui,
            "Журнал",
            "Последние события приложения и ошибки маршрутов. Секреты и содержимое сообщений не отображаются.",
        );
        ui.add_space(10.0);

        egui::Frame::new()
            .fill(egui::Color32::from_rgb(17, 25, 38))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(41, 57, 79)))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Обновить").clicked() {
                        self.refresh_logs();
                    }
                    if ui.button("Очистить экран").clicked() {
                        self.logs_cache.clear();
                    }
                    if ui.button("Открыть папку журналов").clicked() {
                        let _ = std::process::Command::new("explorer")
                            .arg(crate::config::AppConfig::logs_dir())
                            .spawn();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{} строк", self.logs_cache.lines().count())).weak());
                    });
                });
            });

        ui.add_space(8.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(9, 13, 21))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(35, 47, 65)))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                if self.logs_cache.is_empty() {
                    empty_state(ui, "Нет загруженных записей", "Нажмите «Обновить», чтобы прочитать последний журнал.");
                } else {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.logs_cache)
                                        .monospace()
                                        .size(12.0)
                                        .color(egui::Color32::from_rgb(190, 207, 226)),
                                )
                                .selectable(true)
                                .wrap(),
                            );
                        });
                }
            });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        page_heading(
            ui,
            "Настройки",
            "Настройте интеграцию с Telegram, резервные транспорты и поведение приложения в Windows.",
        );
        let mut cfg = self.ctx.config.write();
        let mut save_requested = false;
        ui.collapsing("Общие", |ui| {
            ui.checkbox(&mut cfg.autostart.start_with_windows,"Запускать вместе с Windows");
            ui.checkbox(&mut cfg.autostart.start_connected,"Подключаться при запуске");
            ui.checkbox(&mut cfg.autostart.start_minimized,"Запускать свёрнутым");
            ui.checkbox(&mut cfg.autostart.minimize_to_tray,"Сворачивать в область уведомлений");
            ui.separator();
            ui.checkbox(&mut cfg.telegram.auto_configure,"Добавить прокси в Telegram автоматически");
            ui.label(format!(
                "Регистрация SOCKS5: {}",
                cfg.telegram.configured_proxy.as_deref().and_then(|value| value.strip_prefix("v3:")).unwrap_or("не зарегистрирован")
            ));
            if ui.button("Отметить текущий SOCKS5 как настроенный").clicked() {
                let host = match cfg.proxy.bind.as_str() {
                    "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                    other => other,
                };
                cfg.telegram.configured_proxy = Some(format!("v3:{}:{}", host, cfg.proxy.port));
            }
            if ui.button("Сбросить регистрацию SOCKS5 в Telegram").clicked() {
                cfg.telegram.configured_proxy = None;
            }
            ui.label(format!(
                "Регистрация MTProto: {}",
                if cfg.telegram.configured_mtproto.is_some() { "зарегистрирован" } else { "не зарегистрирован" }
            ));
            if ui.button("Отметить текущий MTProto как настроенный").clicked() {
                cfg.telegram.configured_mtproto = Some(format!(
                    "v4:{}:{}:{}",
                    cfg.mtproto.server.trim(),
                    cfg.mtproto.port,
                    cfg.mtproto.secret.trim()
                ));
            }
            if ui.button("Сбросить регистрацию MTProto в Telegram").clicked() {
                cfg.telegram.configured_mtproto = None;
            }
        });
        ui.collapsing("Telegram", |ui| {
            ui.label("Локальный прокси Telegram, используемый мостом WebSocket:");

            ui.radio_value(
                &mut cfg.telegram.frontend,
                TelegramFrontend::MtprotoWs,
                "MTProto через WebSocket (рекомендуется)",
            );
            ui.radio_value(
                &mut cfg.telegram.frontend,
                TelegramFrontend::Socks5,
                "SOCKS5",
            );

            ui.checkbox(
                &mut cfg.telegram.auto_configure,
                "Настроить Telegram автоматически",
            );

            match cfg.telegram.frontend {
                TelegramFrontend::MtprotoWs => {
                    ui.label("Используется шестнадцатеричный секрет длиной 32 символа. Поддержка старых ссылок Telegram с префиксом dd включена.");
                    ui.small("Telegram может сохранять запись прокси после остановки TeleRoute. Локальный прокси не передаёт трафик после остановки слушателя. Внешний MTProto-прокси нужно отключить непосредственно в Telegram.");
                    ui.horizontal(|ui| {
                        ui.label("Локальный порт");
                        ui.add(egui::Slider::new(&mut cfg.telegram.mtproto_port, 1..=65535));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Секрет");
                        let response = ui.add_sized(
                            [ui.available_width().min(420.0), 28.0],
                            egui::TextEdit::singleline(&mut cfg.telegram.mtproto_secret),
                        );
                        if response.changed() {
                            cfg.telegram.configured_mtproto = None;
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Сгенерировать секрет").clicked() {
                            cfg.telegram.mtproto_secret = generate_random_secret();
                            cfg.telegram.configured_mtproto = None;
                        }

                        if ui.button("Сгенерировать секрет и настроить Telegram").clicked() {
                            cfg.telegram.mtproto_secret = generate_random_secret();
                            cfg.telegram.configured_mtproto = None;

                            #[cfg(windows)]
                            {
                                let host = cfg.telegram.mtproto_bind.clone();
                                let secret = cfg.telegram.mtproto_secret.trim().to_owned();
                                if crate::platform::windows::open_telegram_mtproto_proxy(
                                    &host,
                                    cfg.telegram.mtproto_port,
                                    &secret,
                                ).is_ok() {
                                    cfg.telegram.configured_mtproto = Some(format!(
                                        "v5:{}:{}:{}",
                                        host,
                                        cfg.telegram.mtproto_port,
                                        cfg.telegram.mtproto_secret.trim()
                                    ));
                                }
                            }
                        }
                    });

                    if let Some(proxy) = &cfg.telegram.configured_mtproto {
                        ui.label(format!("Уже настроено: {proxy}"));
                    } else {
                        ui.label("Прокси MTProto ещё не добавлен в Telegram.");
                    }

                    if ui.button("Настроить Telegram сейчас").clicked() {
                        #[cfg(windows)]
                        {
                            let host = cfg.telegram.mtproto_bind.clone();
                            let secret = cfg.telegram.mtproto_secret.trim().to_owned();
                            if crate::platform::windows::open_telegram_mtproto_proxy(
                                &host,
                                cfg.telegram.mtproto_port,
                                &secret,
                            ).is_ok() {
                                cfg.telegram.configured_mtproto = Some(format!(
                                    "v5:{}:{}:{}",
                                    host,
                                    cfg.telegram.mtproto_port,
                                    cfg.telegram.mtproto_secret.trim()
                                ));
                            }
                        }
                    }

                    if ui.button("Сбросить регистрацию MTProto в Telegram").clicked() {
                        cfg.telegram.configured_mtproto = None;
                    }
                }

                TelegramFrontend::Socks5 => {
                    let host = match cfg.proxy.bind.as_str() {
                        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                        other => other,
                    };

                    ui.label("Обычный локальный прокси SOCKS5. Работа медиа и звонков Telegram через SOCKS5 не гарантируется.");
                    ui.label(format!("Адрес SOCKS5: {host}:{}", cfg.proxy.port));

                    if let Some(proxy) = &cfg.telegram.configured_proxy {
                        ui.label(format!("Уже настроено: {proxy}"));
                    } else {
                        ui.label("Прокси SOCKS5 ещё не добавлен в Telegram.");
                    }

                    if ui.button("Настроить Telegram сейчас").clicked() {
                        #[cfg(windows)]
                        {
                            if crate::platform::windows::open_telegram_socks_proxy(host, cfg.proxy.port).is_ok() {
                                cfg.telegram.configured_proxy = Some(format!("{host}:{}", cfg.proxy.port));
                            }
                        }
                    }

                    if ui.button("Сбросить регистрацию SOCKS5 в Telegram").clicked() {
                        cfg.telegram.configured_proxy = None;
                    }
                }
            }
        });
        ui.collapsing("Прокси", |ui| { ui.horizontal(|ui| { ui.label("Адрес"); ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.proxy.bind)); }); ui.add(egui::Slider::new(&mut cfg.proxy.port, 1..=65535).text("Порт")); });
        ui.collapsing("MTProto", |ui| {
            ui.label("Режим MTProto не поддерживает звонки Telegram.");
            ui.horizontal(|ui| {
                ui.label("Сервер");
                ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.mtproto.server));
            });
            ui.add(egui::Slider::new(&mut cfg.mtproto.port, 1..=65535).text("Порт"));
            ui.horizontal(|ui| {
                ui.label("Секрет");
                ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.mtproto.secret));
            });
            ui.label("Пример секрета: 32 шестнадцатеричных символа.");
        });
        ui.collapsing("WebSocket", |ui| {
            ui.add(egui::Slider::new(&mut cfg.timeouts.connect_ms, 500..=15000).text("Тайм-аут подключения (мс)"));
            ui.add(egui::Slider::new(&mut cfg.timeouts.reconnect_ms, 100..=10000).text("Задержка переподключения (мс)"));
            ui.label("Первые попытки WSS ограничены коротким тайм-аутом; затем приложение быстро переходит к резервным адресам.");
            ui.label("Шаблоны адресов задаются в config.toml.");
        });
        ui.collapsing("Маршрутизация", |ui| { for mode in [Mode::Proxy,Mode::Calls,Mode::Full,Mode::Mtproto] { ui.radio_value(&mut cfg.routing.mode, mode, mode_label(mode).to_owned()); } ui.checkbox(&mut cfg.routing.telegram_only,"Маршрутизировать UDP только для Telegram"); ui.checkbox(&mut cfg.routing.direct_udp_fallback,"Резервный прямой UDP"); ui.checkbox(&mut cfg.routing.relay_udp_fallback,"Резервный ретранслятор QUIC"); ui.checkbox(&mut cfg.routing.route_all_traffic,"Маршрутизировать весь трафик (требуется полноценная обработка TCP; пока недоступно)"); });
        ui.collapsing("TUN", |ui| { ui.checkbox(&mut cfg.tun.enabled,"Включить TUN"); ui.add(egui::Slider::new(&mut cfg.tun.mtu, 576..=1500).text("MTU")); ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.tun.adapter_name)); ui.label(format!("Подсети UDP Telegram: {}",cfg.tun.telegram_udp_cidrs.join(", "))); });
        ui.collapsing("Журналы", |ui| { ui.add_sized([ui.available_width().min(360.0), 28.0], egui::TextEdit::singleline(&mut cfg.logging.level)); ui.add(egui::Slider::new(&mut cfg.logging.keep_files,1..=20).text("Хранить файлов")); });
        if ui.add_sized(
            [ui.available_width().min(220.0), 40.0],
            egui::Button::new(egui::RichText::new("Сохранить настройки").strong())
                .fill(egui::Color32::from_rgb(34, 110, 186)),
        ).clicked() {
            save_requested = true;
        }
        drop(cfg);

        if save_requested {
            match self.ctx.save_config() {
                Ok(()) => self.settings_notice = "Настройки успешно сохранены.".into(),
                Err(error) => self.settings_notice = format!("Не удалось сохранить настройки: {error}"),
            }
        }

        if !self.settings_notice.is_empty() {
            ui.add_space(7.0);
            ui.label(egui::RichText::new(&self.settings_notice).color(
                if self.settings_notice.starts_with("Настройки успешно сохранены") {
                    egui::Color32::from_rgb(82, 205, 145)
                } else {
                    egui::Color32::from_rgb(238, 103, 110)
                }
            ));
        }
    }
}

fn generate_random_secret() -> String {
    let mut bytes = [0u8; 16];
    rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Proxy => "Прокси",
        Mode::Calls => "Звонки",
        Mode::Full => "Полный режим",
        Mode::Mtproto => "Внешний MTProto",
    }
}

fn frontend_label(frontend: TelegramFrontend) -> &'static str {
    match frontend {
        TelegramFrontend::MtprotoWs => "MTProto через WebSocket",
        TelegramFrontend::Socks5 => "SOCKS5",
    }
}

fn localize_status(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("CONNECTED · ") {
        return format!("ПОДКЛЮЧЕНО · {}", rest.replace(" active session(s)", " активных сеансов"));
    }
    let mapped = match value {
        "RUNNING" => "ВЫПОЛНЯЕТСЯ",
        "DIAGNOSTIC TASK FAILED" => "СБОЙ ПРОВЕРКИ",
        "OK" => "РАБОТАЕТ",
        "ready" | "READY" => "ГОТОВ",
        "active" | "ACTIVE" => "АКТИВЕН",
        "failed" | "FAILED" => "ОШИБКА",
        "blocked" | "BLOCKED" => "ЗАБЛОКИРОВАН",
        "available" | "AVAILABLE" => "ДОСТУПЕН",
        "unavailable" | "UNAVAILABLE" => "НЕДОСТУПЕН",
        "stopped" | "STOPPED" => "ОСТАНОВЛЕН",
        "inactive" | "INACTIVE" => "НЕАКТИВЕН",
        "unknown" | "UNKNOWN" => "НЕИЗВЕСТНО",
        "not running" => "НЕ ЗАПУЩЕН",
        "NOT REQUIRED" => "НЕ ТРЕБУЕТСЯ",
        "NOT READY" => "НЕ ГОТОВ",
        "NOT LISTENING" => "НЕ СЛУШАЕТ",
        "STOPPED (disconnected)" => "ОСТАНОВЛЕН (отключено)",
        "NOT RUNNING (disconnected)" => "НЕ ЗАПУЩЕН (отключено)",
        "LISTENER READY" => "ПРОКСИ ГОТОВ",
        "CONNECTING" => "ПОДКЛЮЧЕНИЕ",
        "STOPPING" => "ОСТАНОВКА",
        "ERROR" => "ОШИБКА",
        "DISCONNECTED" => "ОТКЛЮЧЕНО",
        "WAITING FOR TELEGRAM CLIENT" => "ОЖИДАНИЕ КЛИЕНТА TELEGRAM",
        "NO VALID TELEGRAM SESSION" => "НЕТ АКТИВНОГО СЕАНСА TELEGRAM",
        "EXTERNAL MTProto HANDOFF" => "ПЕРЕДАЧА ВНЕШНЕМУ MTProto",
        "EXTERNAL PROXY MANAGED BY TELEGRAM" => "ВНЕШНИЙ ПРОКСИ УПРАВЛЯЕТСЯ TELEGRAM",
        "LISTENER STOPPED" => "ПРОКСИ ОСТАНОВЛЕН",
        "LISTENER FAILED TO START" => "НЕ УДАЛОСЬ ЗАПУСТИТЬ ПРОКСИ",
        "STOPPING LOCAL LISTENER" => "ОСТАНОВКА ЛОКАЛЬНОГО ПРОКСИ",
        "DISCONNECTED · LOCAL LISTENER STOPPED" => "ОТКЛЮЧЕНО · ЛОКАЛЬНЫЙ ПРОКСИ ОСТАНОВЛЕН",
        "DISCONNECTED · STARTUP CANCELLED" => "ОТКЛЮЧЕНО · ЗАПУСК ОТМЕНЁН",
        "LISTENER FORCED TO STOP" => "ПРОКСИ ПРИНУДИТЕЛЬНО ОСТАНОВЛЕН",
        "CLIENT SESSIONS STILL CLOSING" => "ЗАКРЫТИЕ СЕАНСОВ КЛИЕНТОВ",
        "MEDIA WSS CHECKING" => "ПРОВЕРКА MEDIA WSS",
        "MEDIA WSS FAILED" => "MEDIA WSS НЕДОСТУПЕН",
        "MEDIA WSS READY · CALL NOT END-TO-END TESTED" => "MEDIA WSS ДОСТУПЕН · ЗВОНКИ ЦЕЛИКОМ НЕ ПРОВЕРЕНЫ",
        "TUN ACTIVE; CALL NOT VERIFIED" => "TUN АКТИВЕН; ЗВОНКИ НЕ ПРОВЕРЕНЫ",
        _ => return value.to_owned(),
    };
    mapped.to_owned()
}

fn localize_error(value: &str) -> String {
    match value {
        "External MTProto requires server, port and secret" =>
            return "Для внешнего MTProto укажите сервер, порт и секрет.".to_owned(),
        "This mode hands an external MTProto server to Telegram. TeleRoute cannot switch off that remote service; disable the selected proxy in Telegram's Connection settings." =>
            return "Этот режим передаёт Telegram адрес внешнего MTProto-прокси. TeleRoute не может отключить удалённый сервер — выключите выбранный прокси в настройках соединения Telegram.".to_owned(),
        "The local listener did not shut down cleanly; its task was stopped forcibly." =>
            return "Локальный прокси не завершился штатно; его задача была остановлена принудительно.".to_owned(),
        _ => {}
    }
    if let Some(rest) = value.strip_prefix("Telegram frontend failed: ") {
        return format!("Не удалось запустить локальный прокси Telegram: {rest}");
    }
    if let Some(rest) = value.strip_prefix("Optional TUN initialization failed: ") {
        return format!("Не удалось запустить дополнительный адаптер TUN: {rest}");
    }
    value.to_owned()
}

fn page_heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.heading(egui::RichText::new(title).size(27.0).strong());
    ui.label(egui::RichText::new(subtitle).size(13.0).weak());
}

fn empty_state(ui: &mut egui::Ui, title: &str, message: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(14.0);
        ui.label(egui::RichText::new(title).size(16.0).strong().color(egui::Color32::from_rgb(177, 200, 226)));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(message).weak());
        ui.add_space(14.0);
    });
}

fn diagnostic_color(value: &str) -> egui::Color32 {
    let lower = value.to_ascii_lowercase();
    if lower.contains("ok") || lower.contains("ready") || lower.contains("listening")
        || lower.contains("available") || lower == "active"
    {
        egui::Color32::from_rgb(82, 205, 145)
    } else if lower.contains("failed") || lower.contains("blocked")
        || lower.contains("unavailable") || lower.contains("error")
    {
        egui::Color32::from_rgb(238, 103, 110)
    } else if lower.contains("stopped") || lower.contains("not running")
        || lower.contains("not listening") || lower.contains("inactive")
    {
        egui::Color32::from_rgb(155, 167, 184)
    } else {
        egui::Color32::from_rgb(242, 189, 91)
    }
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
            ui.label(egui::RichText::new(localize_status(value)).strong().size(15.0));
        });
}
fn human_bytes(n: u64) -> String { const U:[&str;4]=["B","KB","MB","GB"]; let mut v=n as f64; let mut i=0; while v>=1024.0 && i<3 {v/=1024.0;i+=1;} format!("{v:.1} {}",U[i]) }
fn build_tray(ctx: AppContext, egui_ctx: egui::Context) -> anyhow::Result<TrayState> {
    let menu = Menu::new();
    let connect = MenuItem::new("Подключить", true, None);
    let disconnect = MenuItem::new("Отключить", true, None);
    let open = MenuItem::new("Открыть", true, None);
    let configure_telegram = MenuItem::new("Настроить Telegram", true, None);
    let test = MenuItem::new("Проверить", true, None);
    let exit = MenuItem::new("Выход", true, None);
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
