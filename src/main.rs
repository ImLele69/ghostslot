mod attach;
mod auth;
mod commands;
mod protocol;
mod proxy;
mod state;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::auth::Account;
use crate::state::{ClickMode, Dir, Session, Shared, Tag};

struct Args {
    port: Option<String>,
    target: Option<String>,
    autostart: bool,
    verbose: bool,
    stdin: bool,
    join: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: None,
        target: None,
        autostart: false,
        verbose: false,
        stdin: false,
        join: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" | "-p" => args.port = it.next(),
            "--target" | "-t" => args.target = it.next(),
            "--autostart" => args.autostart = true,
            "--verbose" | "-v" => args.verbose = true,
            "--stdin" => args.stdin = true,
            "--no-join" => args.join = false,
            "--help" | "-h" => {
                println!("usage: ghostslot [--target host[:port]] [--port 25599] [--autostart] [--no-join] [--verbose] [--stdin]");
                std::process::exit(0);
            }
            other => eprintln!("ignoring argument: {other}"),
        }
    }
    args
}

fn main() -> eframe::Result {
    let args = parse_args();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ghostslot")
            .with_inner_size([980.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ghostslot",
        options,
        Box::new(move |_cc| {
            let mut app = App::new(rt);
            app.shared.lock().unwrap().echo = args.verbose;
            if let Some(p) = args.port {
                app.listen_port = p;
            }
            if let Some(t) = args.target {
                app.target = t;
            }
            if args.stdin {
                commands::spawn_stdin(app.shared.clone());
            }
            if args.autostart {
                app.start();
                if args.join && app.client.is_some() {
                    app.join_client();
                }
            }
            Ok(Box::new(app))
        }),
    )
}

struct App {
    rt: tokio::runtime::Runtime,
    shared: Shared,
    proxy: Option<tokio::task::JoinHandle<()>>,
    started: Instant,

    listen_port: String,
    target: String,
    accounts: Vec<Account>,
    account: Option<usize>,
    accounts_error: String,
    clients: Vec<attach::McClient>,
    client: Option<usize>,

    click_window: String,
    click_state: String,
    click_auto: bool,
    click_slot: String,
    click_button: String,
    click_mode: ClickMode,
    click_times: String,
    click_delay: bool,

    btn_window: String,
    btn_id: String,
    btn_times: String,
    btn_delay: bool,

    filter: String,
    autoscroll: bool,
}

impl App {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        let (accounts, accounts_error) = match auth::load_prism_accounts() {
            Ok(a) => (a, String::new()),
            Err(e) => (Vec::new(), format!("{e:#}")),
        };
        let mut app_accounts = Vec::new();
        let clients = attach::find_clients();
        // a running client's token is always fresh, so it goes first
        for c in &clients {
            if let Some(a) = c.account() {
                if !app_accounts.iter().any(|x: &Account| x.uuid == a.uuid && x.from_client) {
                    app_accounts.push(a.clone());
                }
            }
        }
        app_accounts.extend(accounts);
        let accounts = app_accounts;
        let account = accounts
            .iter()
            .position(|a| a.from_client)
            .or_else(|| accounts.iter().position(|a| a.active))
            .or(if accounts.is_empty() { None } else { Some(0) });
        let client = clients.iter().position(|c| c.fabric).or(if clients.is_empty() {
            None
        } else {
            Some(0)
        });
        App {
            rt,
            shared: Arc::new(Mutex::new(Session::default())),
            proxy: None,
            started: Instant::now(),
            listen_port: "25599".into(),
            target: "localhost:25565".into(),
            accounts,
            account,
            accounts_error,
            clients,
            client,
            click_window: String::new(),
            click_state: String::new(),
            click_auto: true,
            click_slot: "0".into(),
            click_button: "0".into(),
            click_mode: ClickMode::Pickup,
            click_times: "1".into(),
            click_delay: false,
            btn_window: String::new(),
            btn_id: "0".into(),
            btn_times: "1".into(),
            btn_delay: false,
            filter: String::new(),
            autoscroll: true,
        }
    }

    fn running(&self) -> bool {
        self.proxy.as_ref().is_some_and(|h| !h.is_finished())
    }

    fn start(&mut self) {
        let Ok(port) = self.listen_port.trim().parse() else {
            self.shared.lock().unwrap().info("invalid listen port");
            return;
        };
        let cfg = proxy::Config {
            listen_port: port,
            target: self.target.trim().to_string(),
            account: self.account.and_then(|i| self.accounts.get(i).cloned()),
        };
        let shared = self.shared.clone();
        self.proxy = Some(self.rt.spawn(async move {
            if let Err(e) = proxy::run(cfg, shared.clone()).await {
                shared.lock().unwrap().info(format!("proxy stopped: {e:#}"));
            }
        }));
    }

    fn refresh_clients(&mut self) {
        self.clients = attach::find_clients();
        self.client = self.clients.iter().position(|c| c.fabric).or(if self.clients.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    fn join_client(&self) {
        let Some(client) = self.client.and_then(|i| self.clients.get(i)).cloned() else {
            self.shared.lock().unwrap().info("no running Minecraft client");
            return;
        };
        let address = format!("127.0.0.1:{}", self.listen_port.trim());
        let shared = self.shared.clone();
        shared.lock().unwrap().info(format!("sending {} into the proxy...", client.label()));
        self.rt.spawn_blocking(move || {
            // give the listener a moment to bind when called right after start().
            // 300ms. I could explain the number, but I'd be making it up
            std::thread::sleep(Duration::from_millis(300));
            let msg = match attach::join(&client, &address) {
                Ok(None) => format!("client connecting to {address}"),
                Ok(Some(note)) => format!("client connecting to {address} ({note})"),
                Err(e) => format!("auto join failed: {e:#}"),
            };
            shared.lock().unwrap().info(msg);
        });
    }

    fn stop(&mut self) {
        if let Some(h) = self.proxy.take() {
            h.abort();
        }
        let mut s = self.shared.lock().unwrap();
        s.reset_connection();
        s.status.clear();
        s.info("proxy stopped");
    }

    fn connection_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        ui.horizontal_wrapped(|ui| {
            ui.label("Local port");
            ui.add_enabled(
                !running,
                egui::TextEdit::singleline(&mut self.listen_port).desired_width(60.0),
            );
            ui.label("Server");
            ui.add_enabled(
                !running,
                egui::TextEdit::singleline(&mut self.target).desired_width(220.0),
            );
            ui.label("Account");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let label = |a: &Account| {
                if a.from_client {
                    return format!("{} (running client)", a.name);
                }
                let left = (a.expires - now) / 3600;
                if a.expires > 0 && left < 0 {
                    format!("{} (token expired)", a.name)
                } else {
                    format!("{} ({left}h)", a.name)
                }
            };
            let current = self
                .account
                .and_then(|i| self.accounts.get(i))
                .map(label)
                .unwrap_or_else(|| "none (offline only)".into());
            ui.add_enabled_ui(!running, |ui| {
                egui::ComboBox::from_id_salt("account")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.account, None, "none (offline only)");
                        for (i, a) in self.accounts.iter().enumerate() {
                            ui.selectable_value(&mut self.account, Some(i), label(a));
                        }
                    });
            });
            if running {
                if ui.button("Stop").clicked() {
                    self.stop();
                }
            } else if ui.button("Start proxy").clicked() {
                self.start();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Running client");
            let current = self
                .client
                .and_then(|i| self.clients.get(i))
                .map(|c| c.label())
                .unwrap_or_else(|| "none found".into());
            egui::ComboBox::from_id_salt("client")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (i, c) in self.clients.iter().enumerate() {
                        ui.selectable_value(&mut self.client, Some(i), c.label());
                    }
                });
            if ui.button("Refresh").clicked() {
                self.refresh_clients();
            }
            let fabric = self.client.and_then(|i| self.clients.get(i)).is_some_and(|c| c.fabric);
            if ui
                .add_enabled(running && fabric, egui::Button::new("Join proxy"))
                .on_hover_text("The client leaves its server and joins the proxy (Fabric only)")
                .clicked()
            {
                self.join_client();
            }
        });
        if !self.accounts_error.is_empty() && self.accounts.is_empty() {
            ui.colored_label(Color32::YELLOW, &self.accounts_error);
        }
        let s = self.shared.lock().unwrap();
        let line = if !running {
            "Proxy off".to_string()
        } else if let Some(link) = &s.link {
            format!(
                "Connected: {}  [{}]",
                s.status,
                if link.cb_play && link.sb_play { "play" } else { "login/config" }
            )
        } else {
            format!(
                "Waiting: add the server 127.0.0.1:{} in Minecraft",
                self.listen_port.trim()
            )
        };
        ui.label(RichText::new(line).strong());
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let shared = self.shared.clone();
        let mut s = shared.lock().unwrap();
        let play = s.in_play();

        ui.heading("GUI state");
        egui::Grid::new("status").num_columns(2).show(ui, |ui| {
            ui.label("Client screen");
            ui.label(match &s.client_screen {
                Some(c) => format!("#{} type {} \"{}\"", c.id, c.kind, c.title),
                None => "none".into(),
            });
            ui.end_row();
            ui.label("Server container");
            ui.label(match s.server_container {
                Some(id) => format!("#{id}"),
                None => "none (or inventory #0)".into(),
            });
            ui.end_row();
            ui.label("Revision (stateId)");
            ui.label(s.state_id.to_string());
            ui.end_row();
        });
        let desynced = match (&s.client_screen, s.server_container) {
            (Some(c), Some(id)) => c.id != id,
            (Some(_), None) | (None, Some(_)) => true,
            _ => false,
        };
        if desynced {
            ui.colored_label(Color32::LIGHT_RED, "Client and server are out of sync");
        }

        ui.separator();
        ui.add_enabled_ui(play, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("Close without packet")
                    .on_hover_text("Closes the GUI on the client, the server keeps it open")
                    .clicked()
                {
                    s.close_without_packet();
                }
                if ui
                    .button("De-sync")
                    .on_hover_text("Closes the container on the server, the client still shows it")
                    .clicked()
                {
                    s.desync();
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("Save GUI").clicked() {
                    s.save_gui();
                }
                let can_load = s.saved_open.is_some();
                if ui.add_enabled(can_load, egui::Button::new("Load GUI")).clicked() {
                    s.load_gui();
                }
                if can_load {
                    ui.label(format!("saved: {}", s.saved_label));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("Unload chunks")
                    .on_hover_text("The client unloads every chunk, the server keeps them loaded")
                    .clicked()
                {
                    s.unload_chunks();
                }
                ui.label(format!("loaded chunks: {}", s.chunks.len()));
            });
        });

        ui.separator();
        let mut send = !s.drop_packets;
        if ui
            .checkbox(&mut send, "Send packets")
            .on_hover_text("When off, the client's clicks and button clicks are dropped")
            .changed()
        {
            s.drop_packets = !send;
        }
        ui.checkbox(&mut s.delay_packets, "Delay packets").on_hover_text(
            "When on, the client's clicks and button clicks wait in the queue until you send them",
        );
        let queued = s.delayed.len();
        ui.add_enabled_ui(play, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Queued: {queued}"));
                if ui.add_enabled(queued > 0, egui::Button::new("Send queue")).clicked() {
                    s.flush_delayed();
                }
                if ui.add_enabled(queued > 0, egui::Button::new("Discard queue")).clicked() {
                    s.delayed.clear();
                    s.info("queue discarded");
                }
            });
            if ui
                .button("Disconnect and send packets")
                .on_hover_text("Sends the queue and closes the server connection right after")
                .clicked()
            {
                s.disconnect_and_send();
            }
        });

        ui.separator();
        ui.heading("Fabricate: container_click");
        let current_window = s.client_screen.as_ref().map(|c| c.id).unwrap_or(0);
        let current_state = s.state_id;
        egui::Grid::new("click").num_columns(2).show(ui, |ui| {
            ui.label("Window id");
            ui.add(
                egui::TextEdit::singleline(&mut self.click_window)
                    .hint_text(current_window.to_string())
                    .desired_width(80.0),
            );
            ui.end_row();
            ui.label("Revision");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.click_auto, "current");
                ui.add_enabled(
                    !self.click_auto,
                    egui::TextEdit::singleline(&mut self.click_state)
                        .hint_text(current_state.to_string())
                        .desired_width(60.0),
                );
            });
            ui.end_row();
            ui.label("Slot");
            ui.add(egui::TextEdit::singleline(&mut self.click_slot).desired_width(80.0));
            ui.end_row();
            ui.label("Button");
            ui.add(egui::TextEdit::singleline(&mut self.click_button).desired_width(80.0));
            ui.end_row();
            ui.label("Mode");
            egui::ComboBox::from_id_salt("mode")
                .selected_text(self.click_mode.name())
                .show_ui(ui, |ui| {
                    for m in ClickMode::ALL {
                        ui.selectable_value(&mut self.click_mode, m, m.name());
                    }
                });
            ui.end_row();
            ui.label("Repeat");
            ui.add(egui::TextEdit::singleline(&mut self.click_times).desired_width(80.0));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.click_delay, "queue");
            if ui.add_enabled(play, egui::Button::new("Send click")).clicked() {
                let window = parse_or(&self.click_window, current_window);
                let state = if self.click_auto {
                    current_state
                } else {
                    parse_or(&self.click_state, current_state)
                };
                match (self.click_slot.trim().parse(), self.click_button.trim().parse()) {
                    (Ok(slot), Ok(button)) => s.fabricate_click(
                        window,
                        state,
                        slot,
                        button,
                        self.click_mode,
                        parse_or(&self.click_times, 1u32).clamp(1, 500),
                        self.click_delay,
                    ),
                    _ => s.info("invalid slot or button"),
                }
            }
        });

        ui.separator();
        ui.heading("Fabricate: button_click");
        ui.horizontal_wrapped(|ui| {
            ui.label("Window");
            ui.add(
                egui::TextEdit::singleline(&mut self.btn_window)
                    .hint_text(current_window.to_string())
                    .desired_width(50.0),
            );
            ui.label("Button");
            ui.add(egui::TextEdit::singleline(&mut self.btn_id).desired_width(50.0));
            ui.label("Repeat");
            ui.add(egui::TextEdit::singleline(&mut self.btn_times).desired_width(50.0));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.btn_delay, "queue");
            if ui.add_enabled(play, egui::Button::new("Send button")).clicked() {
                match self.btn_id.trim().parse() {
                    Ok(button) => s.fabricate_button(
                        parse_or(&self.btn_window, current_window),
                        button,
                        parse_or(&self.btn_times, 1u32).clamp(1, 500),
                        self.btn_delay,
                    ),
                    Err(_) => s.info("invalid button"),
                }
            }
        });
    }

    fn log_view(&mut self, ui: &mut egui::Ui) {
        let shared = self.shared.clone();
        let mut s = shared.lock().unwrap();
        let started = self.started;
        ui.horizontal(|ui| {
            ui.heading("Packets");
            ui.checkbox(&mut s.log_all, "all");
            ui.checkbox(&mut self.autoscroll, "follow");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("filter")
                    .desired_width(140.0),
            );
            if ui.button("Clear").clicked() {
                s.log.clear();
            }
            if ui.button("Copy").clicked() {
                let text: String = s
                    .log
                    .iter()
                    .map(|l| format!("{}\n", format_line(started, l).0))
                    .collect();
                ui.ctx().copy_text(text);
            }
        });
        let filter = self.filter.to_lowercase();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(self.autoscroll)
            .show(ui, |ui| {
                for line in s.log.iter() {
                    if !filter.is_empty() && !line.text.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let (text, color) = format_line(started, line);
                    ui.label(RichText::new(text).monospace().color(color));
                }
            });
    }
}

fn format_line(started: Instant, l: &state::LogLine) -> (String, Color32) {
    let t = l.at.saturating_duration_since(started).as_secs_f32();
    let dir = match l.dir {
        Some(Dir::ToServer) => "C→S",
        Some(Dir::ToClient) => "S→C",
        None => "   ",
    };
    let (tag, color) = match l.tag {
        Tag::Pass => ("", Color32::GRAY),
        Tag::Delayed => ("[QUEUED] ", Color32::from_rgb(230, 180, 60)),
        Tag::Dropped => ("[DROPPED] ", Color32::LIGHT_RED),
        Tag::Injected => ("[INJECTED] ", Color32::from_rgb(110, 200, 255)),
        Tag::Info => ("", Color32::from_rgb(150, 220, 150)),
    };
    (format!("{t:9.3} {dir} {tag}{}", l.text), color)
}

fn parse_or<T: std::str::FromStr>(s: &str, default: T) -> T {
    let s = s.trim();
    if s.is_empty() {
        default
    } else {
        s.parse().unwrap_or(default)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        egui::Panel::top("conn").show(ui, |ui| {
            ui.add_space(4.0);
            self.connection_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::left("controls")
            .resizable(true)
            .default_size(380.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui));
            });
        egui::CentralPanel::default_margins().show(ui, |ui| self.log_view(ui));
    }
}
