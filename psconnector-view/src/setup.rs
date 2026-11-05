//! The start screen: find consoles on the network, sign in to PSN, pair with a PS5 (PIN), wake it from rest
//! mode, and connect. Everything slow runs on the tokio runtime and reports back through a channel.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use eframe::egui;
use psconnector::{Client, Options, ddp, profile, psn, register};

use crate::theme;

struct Console {
    addr: String,
    name: String,
    host_id: Option<String>,
    on: bool,
    registered: bool,
}

enum Msg {
    Found(Vec<ddp::Found>),
    /// A console typed in by address answered (or not)
    Status(String, Result<ddp::Status, String>),
    LoggedIn(Result<String, String>),
    /// The sign-in window finished: the redirect address, or why not
    SignInWindow(Result<String, String>),
    Paired(String, Result<(), String>),
    Note(String),
    Connected(Result<(String, Client), String>),
}

/// Something a button asked for, run after drawing (so drawing can borrow the screen's state)
type Action = Box<dyn FnOnce(&mut Setup)>;

pub struct Setup {
    rt: tokio::runtime::Handle,
    opts: Options,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    consoles: Vec<Console>,
    searching: bool,
    manual: String,
    /// Signed-in account, and whether it comes from pyremoteplay's file
    account: Option<(profile::User, bool)>,
    confirm: Option<Confirm>,
    /// Showing the "paste the address" step of the PSN sign-in
    login: Option<String>,
    /// Pairing with this console: (address, PIN typed so far)
    pair: Option<(String, String)>,
    busy: Option<String>,
    error: Option<String>,
}

fn registered(host_id: Option<&str>) -> bool {
    host_id.is_some_and(|id| profile::load_any(None, id).is_ok())
}

/// The signed-in account: ours first, else one signed in earlier with pyremoteplay (`true`).
fn first_account() -> Option<(profile::User, bool)> {
    let ours = profile::users(&profile::default_path()).into_iter().next().map(|u| (u, false));
    ours.or_else(|| {
        profile::read_pyremoteplay()
            .then(|| profile::users(&profile::pyremoteplay_path()).into_iter().next().map(|u| (u, true)))
            .flatten()
    })
}

/// Stops reading pyremoteplay's registrations (its file itself is left alone).
fn stop_reading_pyremoteplay() {
    profile::set_read_pyremoteplay(false);
    crate::settings::set("read_pyremoteplay", false.into());
}

/// A step that needs a second click.
enum Confirm {
    SignOut,
    Forget { host_id: String, name: String },
}

/// Opens a web page in the default browser.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        log::warn!("cannot open the browser: {e}");
    }
}

impl Setup {
    /// `host`: connect to this console right away (from the command line, or the last one used).
    pub fn new(rt: tokio::runtime::Handle, opts: Options, host: Option<String>) -> Self {
        let (tx, rx) = channel();
        let mut s = Self {
            rt,
            opts,
            tx,
            rx,
            consoles: Vec::new(),
            searching: false,
            manual: String::new(),
            account: first_account(),
            confirm: None,
            login: None,
            pair: None,
            busy: None,
            error: None,
        };
        match host {
            Some(h) => s.connect(h),
            None => s.search(),
        }
        s
    }

    fn search(&mut self) {
        self.searching = true;
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let found = ddp::discover(Duration::from_secs(3)).await.unwrap_or_default();
            let _ = tx.send(Msg::Found(found));
        });
    }

    fn add_status(&mut self, addr: &str, st: &ddp::Status, name: Option<&str>) {
        let host_id = st.host_id().map(str::to_string);
        let c = Console {
            addr: addr.to_string(),
            name: name.or(st.fields.get("host-name").map(String::as_str)).unwrap_or("PS5").to_string(),
            registered: registered(host_id.as_deref()),
            host_id,
            on: st.is_on(),
        };
        match self.consoles.iter_mut().find(|x| x.addr == c.addr || (x.host_id.is_some() && x.host_id == c.host_id)) {
            Some(x) => *x = c,
            None => self.consoles.push(c),
        }
    }

    /// Checks the console, and connects if it is on and registered (else the screen shows what to do).
    fn connect(&mut self, addr: String) {
        self.error = None;
        self.busy = Some(format!("connecting to {addr}..."));
        let (tx, opts) = (self.tx.clone(), self.opts.clone());
        self.rt.spawn(async move {
            let st = match ddp::status(&addr).await {
                Ok(st) => st,
                Err(e) => {
                    let _ = tx.send(Msg::Status(addr, Err(e.to_string())));
                    return;
                }
            };
            let ready = st.is_on() && registered(st.host_id());
            let _ = tx.send(Msg::Status(addr.clone(), Ok(st)));
            if ready {
                let r = crate::connect(&addr, opts).await.map(|c| (addr, c)).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Connected(r));
            }
        });
    }

    fn wake(&mut self, addr: String, host_id: String) {
        self.error = None;
        self.busy = Some(format!("waking {addr}..."));
        let (tx, opts) = (self.tx.clone(), self.opts.clone());
        self.rt.spawn(async move {
            let result: Result<(), String> = async {
                let reg = profile::load_any(None, &host_id).map_err(|e| e.to_string())?;
                ddp::wakeup(&addr, &reg.regist_key).await.map_err(|e| e.to_string())?;
                for _ in 0..45 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if ddp::status(&addr).await.is_ok_and(|s| s.is_on()) {
                        return Ok(());
                    }
                }
                Err("the PS5 did not wake up within 90 s".into())
            }
            .await;
            match result {
                Ok(()) => {
                    let _ = tx.send(Msg::Note(format!("PS5 is on; connecting to {addr}...")));
                    // the console needs a few seconds after waking before it takes a session
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let r = crate::connect(&addr, opts).await.map(|c| (addr, c)).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Connected(r));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Connected(Err(e)));
                }
            }
        });
    }

    /// Signs in through the in-app window; falls back to the browser if it cannot open.
    fn sign_in(&mut self) {
        self.error = None;
        self.login = None;
        self.busy = Some("sign in to PSN in the window that opened...".into());
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            let _ = tx.send(Msg::SignInWindow(crate::login_window::sign_in()));
        });
    }

    fn sign_in_with_browser(&mut self) {
        open_browser(&psn::login_url());
        self.login = Some(String::new());
    }

    fn finish_login(&mut self, redirect: String) {
        self.error = None;
        self.busy = Some("signing in...".into());
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let r = async {
                let acc = psn::account_from_redirect(&redirect).await.map_err(|e| e.to_string())?;
                profile::save_account(&profile::default_path(), &acc.online_id, &acc.account_id)
                    .map_err(|e| e.to_string())?;
                Ok(acc.online_id)
            }
            .await;
            let _ = tx.send(Msg::LoggedIn(r));
        });
    }

    fn refresh_pairing(&mut self) {
        self.account = first_account();
        for c in &mut self.consoles {
            c.registered = registered(c.host_id.as_deref());
        }
    }

    fn sign_out(&mut self) {
        if let Some((acc, from_pyremoteplay)) = self.account.take() {
            if from_pyremoteplay {
                stop_reading_pyremoteplay();
            } else if let Err(e) = profile::remove_account(&profile::default_path(), &acc.online_id) {
                self.error = Some(format!("cannot sign out: {e}"));
            }
        }
        self.refresh_pairing();
    }

    fn forget(&mut self, host_id: &str) {
        if let Err(e) = profile::remove_host(&profile::default_path(), host_id) {
            self.error = Some(format!("cannot forget the console: {e}"));
        }
        // still paired through pyremoteplay's file: stop reading that file
        if registered(Some(host_id)) {
            stop_reading_pyremoteplay();
        }
        self.refresh_pairing();
    }

    fn pair_with(&mut self, addr: String, pin: String) {
        let Some((acc, _)) = self.account.clone() else { return };
        self.error = None;
        self.busy = Some(format!("pairing with {addr}..."));
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let r = async {
                let st = ddp::status(&addr).await.map_err(|e| e.to_string())?;
                let host_id = st.host_id().ok_or("the console sent no host-id")?.to_string();
                let data = register::register(&addr, &acc.account_id, &pin).await.map_err(|e| e.to_string())?;
                profile::save_registration(&profile::default_path(), &acc.online_id, &acc.account_id, &host_id, &data)
                    .map_err(|e| e.to_string())
            }
            .await;
            let _ = tx.send(Msg::Paired(addr, r));
        });
    }

    fn handle(&mut self, msg: Msg) -> Option<(String, Client)> {
        match msg {
            Msg::Found(found) => {
                self.searching = false;
                for f in &found {
                    self.add_status(&f.addr.to_string(), &f.status, Some(f.name()));
                }
            }
            Msg::Status(addr, Ok(st)) => {
                let (on, reg) = (st.is_on(), registered(st.host_id()));
                self.add_status(&addr, &st, None);
                if !on || !reg {
                    self.busy = None;
                    self.error = Some(if !reg && !on {
                        format!(
                            "{addr} is not paired with this computer yet. Turn the PS5 on (power button or its controller), then press Pair"
                        )
                    } else if !reg {
                        format!("{addr} is not paired with this computer yet: press Pair")
                    } else {
                        format!("{addr} is in rest mode: press Wake")
                    });
                    if self.consoles.len() == 1 {
                        self.search(); // also look for others
                    }
                }
            }
            Msg::Status(addr, Err(e)) => {
                self.busy = None;
                self.error = Some(format!("{addr}: {e}"));
                if self.consoles.is_empty() && !self.searching {
                    self.search();
                }
            }
            Msg::SignInWindow(Ok(url)) => self.finish_login(url),
            Msg::SignInWindow(Err(e)) => {
                self.busy = None;
                if e != "sign-in window closed" {
                    log::warn!("{e}");
                    self.sign_in_with_browser();
                }
            }
            Msg::LoggedIn(r) => {
                self.busy = None;
                match r {
                    Ok(name) => {
                        self.login = None;
                        self.refresh_pairing();
                        self.error = None;
                        log::info!("signed in as {name}");
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Msg::Paired(addr, r) => {
                self.busy = None;
                match r {
                    Ok(()) => {
                        self.pair = None;
                        self.refresh_pairing();
                        self.connect(addr);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Msg::Note(n) => self.busy = Some(n),
            Msg::Connected(Ok(c)) => {
                crate::settings::save_host(&c.0);
                return Some(c);
            }
            Msg::Connected(Err(e)) => {
                self.busy = None;
                self.error = Some(e);
            }
        }
        None
    }

    /// Draws the screen. Returns the connection once a console is connected.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<(String, Client)> {
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(done) = self.handle(msg) {
                return Some(done);
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        let idle = self.busy.is_none();
        let mut action: Option<Action> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // one centered column, like a shadcn page
                let width = ui.available_width().min(580.0);
                let pad = ((ui.available_width() - width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        self.draw(ui, idle, &mut action);
                    });
                });
            });
        });
        if let Some(a) = action {
            a(self);
        }
        None
    }

    fn draw(&mut self, ui: &mut egui::Ui, idle: bool, action: &mut Option<Action>) {
        ui.add_space(28.0);
        ui.heading(egui::RichText::new("PsConnector").strong());
        ui.label(theme::muted(ui, "Connect to a PS5 on this network."));
        ui.add_space(16.0);

        // ---- account
        let (title, desc) = match &self.account {
            Some((a, _)) => ("PSN account", format!("Signed in as {}", a.online_id)),
            None => ("PSN account", "Needed once, to pair this computer with a PS5.".to_string()),
        };
        theme::card(ui, title, Some(&desc), |ui| {
            ui.horizontal(|ui| {
                let label = if self.account.is_some() { "Sign in again" } else { "Sign in to PSN" };
                let main = if self.account.is_some() {
                    ui.add_enabled(idle, egui::Button::new(label))
                } else {
                    theme::primary(ui, idle, label)
                };
                if main.clicked() {
                    *action = Some(Box::new(|s: &mut Self| s.sign_in()));
                }
                if theme::ghost(ui, idle, "Use the browser")
                    .on_hover_text("Sign in in your web browser and paste the address back")
                    .clicked()
                {
                    *action = Some(Box::new(|s: &mut Self| s.sign_in_with_browser()));
                }
                if self.account.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::ghost(ui, idle, "Sign out").clicked() {
                            *action = Some(Box::new(|s: &mut Self| s.confirm = Some(Confirm::SignOut)));
                        }
                    });
                }
            });
            if let Some(text) = &mut self.login {
                ui.add_space(6.0);
                ui.label("1. Sign in on Sony's page that opened in your browser. The password goes only to Sony.");
                ui.horizontal(|ui| {
                    if theme::ghost(ui, true, "Copy the sign-in link").clicked() {
                        ui.ctx().copy_text(psn::login_url());
                    }
                    ui.label(theme::muted(ui, "if no browser opened"));
                });
                ui.label("2. Paste the address of the page you land on (…/remoteplay/redirect?code=…):");
                let mut submit = false;
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 150.0).max(160.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(text)
                            .desired_width(w)
                            .hint_text("https://remoteplay.dl.playstation.net/remoteplay/redirect?code=..."),
                    );
                    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    submit |= theme::primary(ui, idle && !text.trim().is_empty(), "OK").clicked();
                    if ui.button("Cancel").clicked() {
                        *action = Some(Box::new(|s: &mut Self| s.login = None));
                    }
                });
                if submit && !text.trim().is_empty() {
                    let url = text.trim().to_string();
                    *action = Some(Box::new(move |s: &mut Self| s.finish_login(url)));
                }
            }
        });
        ui.add_space(12.0);

        // ---- consoles
        theme::card(ui, "Consoles", Some("PS5s that answered on this network, on or in rest mode."), |ui| {
            if self.consoles.is_empty() {
                ui.label(theme::muted(
                    ui,
                    if self.searching {
                        "Searching..."
                    } else {
                        "No PS5 found. Check that it is on (or in rest mode with Remote Play enabled) and on this network, or type its address below."
                    },
                ));
            }
            for c in &self.consoles {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&c.name).strong());
                        ui.label(theme::muted(ui, &c.addr).size(12.0));
                    });
                    theme::badge(ui, if c.on { "on" } else { "rest mode" }, c.on);
                    theme::badge(ui, if c.registered { "paired" } else { "not paired" }, false);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (addr, id) = (c.addr.clone(), c.host_id.clone());
                        if !c.registered {
                            let can = idle && self.account.is_some() && c.on;
                            if theme::primary(ui, can, "Pair")
                                .on_disabled_hover_text("Sign in to PSN first, and turn the PS5 on")
                                .clicked()
                            {
                                *action = Some(Box::new(move |s: &mut Self| s.pair = Some((addr, String::new()))));
                            }
                        } else if !c.on {
                            if theme::primary(ui, idle, "Wake").clicked() {
                                *action = Some(Box::new(move |s: &mut Self| s.wake(addr, id.unwrap_or_default())));
                            }
                        } else if theme::primary(ui, idle, "Connect").clicked() {
                            *action = Some(Box::new(move |s: &mut Self| s.connect(addr)));
                        }
                        if c.registered
                            && let Some(id) = c.host_id.clone()
                            && theme::ghost(ui, idle, "Forget")
                                .on_hover_text("Remove this console's pairing from this computer")
                                .clicked()
                        {
                            let name = c.name.clone();
                            *action = Some(Box::new(move |s: &mut Self| {
                                s.confirm = Some(Confirm::Forget { host_id: id, name })
                            }));
                        }
                    });
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.manual)
                        .desired_width(170.0)
                        .hint_text("Address, e.g. 192.168.1.20"),
                );
                if ui.add_enabled(idle && !self.manual.trim().is_empty(), egui::Button::new("Connect")).clicked() {
                    let addr = self.manual.trim().to_string();
                    *action = Some(Box::new(move |s: &mut Self| s.connect(addr)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.searching {
                        ui.spinner();
                    } else if theme::ghost(ui, idle, "Search again").clicked() {
                        *action = Some(Box::new(|s: &mut Self| s.search()));
                    }
                });
            });
        });

        // ---- pairing
        if let Some((addr, pin)) = &mut self.pair {
            ui.add_space(12.0);
            theme::card(
                ui,
                &format!("Pair with {addr}"),
                Some(
                    "On the PS5: Settings → System → Remote Play → Pair Device, then type the 8-digit number it shows.",
                ),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(pin).desired_width(120.0).char_limit(8).hint_text("12345678"),
                        );
                        pin.retain(|c| c.is_ascii_digit());
                        let (a, p) = (addr.clone(), pin.clone());
                        if theme::primary(ui, idle && p.len() == 8, "Pair").clicked() {
                            *action = Some(Box::new(move |s: &mut Self| s.pair_with(a, p)));
                        }
                        if ui.button("Cancel").clicked() {
                            *action = Some(Box::new(|s: &mut Self| s.pair = None));
                        }
                    });
                },
            );
        }

        // ---- second click for things that cannot be undone here
        if let Some(c) = &self.confirm {
            ui.add_space(12.0);
            let (title, text, yes) = match c {
                Confirm::SignOut => (
                    "Sign out?".to_string(),
                    "Consoles paired with this account on this computer will need pairing again.".to_string(),
                    "Sign out",
                ),
                Confirm::Forget { name, .. } => (
                    format!("Forget {name}?"),
                    "This computer will need pairing again. The PS5 still lists it under Settings → System → Remote Play → Paired Devices; remove it there too if you like.".to_string(),
                    "Forget",
                ),
            };
            theme::card(ui, &title, Some(&text), |ui| {
                ui.horizontal(|ui| {
                    if theme::destructive(ui, true, yes).clicked() {
                        *action = Some(Box::new(|s: &mut Self| match s.confirm.take() {
                            Some(Confirm::SignOut) => s.sign_out(),
                            Some(Confirm::Forget { host_id, .. }) => s.forget(&host_id),
                            None => {}
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        *action = Some(Box::new(|s: &mut Self| s.confirm = None));
                    }
                });
            });
        }

        ui.add_space(14.0);
        if let Some(b) = &self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(theme::muted(ui, b));
            });
        }
        if let Some(e) = &self.error {
            theme::error(ui, e);
        }
    }
}
