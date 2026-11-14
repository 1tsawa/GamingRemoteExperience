//! Interface egui : onglets Client, Host et Paramètres (`settings_tab.rs`). Aucune E/S ici : tout passe par `backend`.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, vec2, Align, Layout, RichText, ScrollArea, TextEdit, TextureHandle, TextureOptions};
use grx_core::util::now_ms;
use zeroize::Zeroize;

use crate::backend::{
    self, ClientSessions, DataCmd, Event, HostCmd, HostStatus, InviteCreated, Level, Paths, ProfileRow, SessionState,
    Sink,
};
use crate::logic::{
    self, format_countdown, format_date_local, remaining_secs, validate_addr, validate_link, validate_name, ClientListState,
    ClientRow, CodecChoice, LinkCheck, Ttl, DEFAULT_PORT,
};
use crate::hoststatus::{self, RunStatus, Tone};

#[path = "settings_tab.rs"]
mod settings_tab;
pub use settings_tab::DecoderProbe;
use crate::qr::QrMatrix;
use crate::settings::{self, GuiSettings};
use crate::update::{self, BannerAction, MenuAction, UpdateUi};
use crate::theme::{self, card, colored, danger_button, dot, muted, primary_button, small_muted, ERR, MUTED, OK, WARN};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Client,
    Host,
    Settings,
}

/// Options de démonstration / test (cachées dans la ligne de commande).
#[derive(Default, Clone)]
pub struct StartOptions {
    pub tab: Tab,
    pub prefill_link: Option<String>,
    pub prefill_addr: Option<String>,
    pub auto_invite: bool,
    pub auto_copy_invite: bool,
    pub auto_start_host: bool,
    pub quit_without_confirm: bool,
    /// Force `--no-input --no-audio` pour le host lancé (tests).
    pub force_safe_host: bool,
    /// Ouvre la fenêtre « commande pare-feu » dès que le host tourne (capture d'écran de test).
    pub auto_firewall_help: bool,
    /// Ouvert par un lien grx:// (association de protocole) : pas de vérification de mise à jour.
    pub opened_from_link: bool,
}

struct ActiveInvite {
    uri: String,
    uri_v6: Option<String>,
    addr: String,
    expires_ms: u64,
    texture: Option<TextureHandle>,
    qr_error: Option<String>,
}

impl Drop for ActiveInvite {
    fn drop(&mut self) {
        self.uri.zeroize();
        if let Some(u) = self.uri_v6.as_mut() {
            u.zeroize();
        }
    }
}

struct Notice {
    level: Level,
    text: String,
    at: Instant,
}

pub struct App {
    paths: Paths,
    sink: Sink,
    events: Receiver<Event>,
    data_tx: Sender<DataCmd>,
    host_tx: Sender<HostCmd>,
    sessions: ClientSessions,
    opts: StartOptions,

    tab: Tab,
    settings: GuiSettings,
    notice: Option<Notice>,

    // Host
    host_fp: Option<Result<String, String>>,
    clients: ClientListState,
    clients_err: Option<String>,
    invite: Option<ActiveInvite>,
    invite_note: Option<String>,
    invite_pending: bool,
    host: HostStatus,
    host_log: Vec<String>,
    /// État du mode Contrôle total (tâche planifiée).
    elevation: backend::ElevationState,
    revoke_confirm: Option<ClientRow>,
    /// Confirmation avant de donner le rôle « Contrôle total » à un client.
    total_confirm: Option<ClientRow>,
    detect_ip_pending: bool,
    auto_addr_pending: bool,
    run_status: Option<(RunStatus, Duration)>,
    firewall_help: Option<Result<String, String>>,
    firewall_help_pending: bool,

    // Client
    client_fp: Option<Result<String, String>>,
    profiles: Vec<ProfileRow>,
    profiles_err: Option<String>,
    profiles_loaded: bool,
    link_text: String,
    name_text: String,
    name_touched: bool,
    pair_pending: Option<String>,
    rename: Option<(String, String)>,
    delete_confirm: Option<String>,
    running_sessions: Vec<(u64, String)>,

    // Fermeture
    quit_confirm: bool,
    allow_close: bool,
    auto_done: bool,
    last_link_valid: bool,
    /// Images restantes pendant lesquelles on fait défiler jusqu'au bouton « Appairer et connecter »
    /// (la mise en page n'est stable qu'après une ou deux images).
    scroll_to_pair: u8,

    // Mises à jour
    update: UpdateUi,

    // Paramètres : sonde des décodeurs de ce PC (`grx-client decoders`), lancée à la demande.
    decoders: DecoderProbe,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths, opts: StartOptions) -> Self {
        theme::setup(&cc.egui_ctx);
        let (sink, events) = Sink::new(cc.egui_ctx.clone());
        let (data_tx, data_rx) = channel();
        let (host_tx, host_rx) = channel();
        backend::spawn_data_worker(paths.clone(), sink.clone(), data_rx);
        backend::spawn_host_manager(paths.clone(), sink.clone(), host_rx);

        let mut settings = settings::load(&settings::settings_path(&paths.host_dir));
        if let Some(a) = &opts.prefill_addr {
            settings.public_addr = a.clone();
            settings.public_addr_auto = false;
        }
        if opts.force_safe_host {
            settings.host.no_input = true;
            settings.host.no_audio = true;
        }
        let link_text = opts.prefill_link.clone().unwrap_or_default();
        Self {
            paths,
            sink,
            events,
            data_tx,
            host_tx,
            sessions: ClientSessions::default(),
            tab: opts.tab,
            settings,
            notice: None,
            host_fp: None,
            clients: ClientListState::default(),
            clients_err: None,
            invite: None,
            invite_note: None,
            invite_pending: false,
            host: HostStatus::default(),
            elevation: backend::ElevationState::default(),
            host_log: Vec::new(),
            revoke_confirm: None,
            total_confirm: None,
            detect_ip_pending: false,
            auto_addr_pending: false,
            run_status: None,
            firewall_help: None,
            firewall_help_pending: false,
            client_fp: None,
            profiles: Vec::new(),
            profiles_err: None,
            profiles_loaded: false,
            link_text,
            name_text: String::new(),
            name_touched: false,
            pair_pending: None,
            rename: None,
            delete_confirm: None,
            running_sessions: Vec::new(),
            quit_confirm: false,
            allow_close: false,
            auto_done: false,
            last_link_valid: false,
            scroll_to_pair: 0,
            update: UpdateUi::default(),
            decoders: DecoderProbe::default(),
            opts,
        }
    }

    fn notify(&mut self, level: Level, text: impl Into<String>) {
        self.notice = Some(Notice { level, text: text.into(), at: Instant::now() });
    }

    fn save_settings(&self) {
        let _ = self.data_tx.send(DataCmd::SaveSettings(self.settings.clone()));
    }

    // -----------------------------------------------------------------------------------------
    // Événements du backend
    // -----------------------------------------------------------------------------------------

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                Event::HostIdentity(r) => self.host_fp = Some(r),
                Event::ClientIdentity(r) => self.client_fp = Some(r),
                Event::Clients(Ok(rows)) => {
                    self.clients_err = None;
                    let appeared = self.clients.update(rows, Instant::now());
                    for r in appeared {
                        self.notify(Level::Success, format!("Nouveau client autorisé : « {} » ({})", r.name, r.fingerprint));
                        if self.invite.is_some() {
                            // Invitation à usage unique : elle vient d'être consommée, on efface le secret.
                            self.invite = None;
                            self.invite_note = Some(format!("Invitation utilisée par « {} » : elle n'est plus valable.", r.name));
                        }
                    }
                }
                Event::Clients(Err(e)) => self.clients_err = Some(e),
                Event::Profiles(Ok(rows)) => {
                    self.profiles_err = None;
                    self.profiles_loaded = true;
                    if let Some(name) = &self.pair_pending {
                        if rows.iter().any(|p| &p.name == name) {
                            // Le host est mémorisé : l'appairage a réussi, le lien n'est plus utile.
                            self.link_text.zeroize();
                            self.link_text.clear();
                            self.name_text.clear();
                            self.name_touched = false;
                            let n = name.clone();
                            self.pair_pending = None;
                            self.notify(Level::Success, format!("Host « {n} » appairé et mémorisé."));
                        }
                    }
                    self.profiles = rows;
                }
                Event::Profiles(Err(e)) => {
                    self.profiles_loaded = true;
                    self.profiles_err = Some(e);
                }
                Event::InviteCreated(ic) => self.on_invite_created(ctx, ic),
                Event::Notice(level, text) => {
                    if level == Level::Error {
                        self.invite_pending = false;
                    }
                    self.notify(level, text)
                }
                Event::LocalIp(addr) => {
                    self.detect_ip_pending = false;
                    self.settings.public_addr = addr;
                }
                Event::Host(st) => self.host = st,
                Event::HostLog(lines) => self.host_log = lines,
                Event::Session { id, name, state } => match state {
                    SessionState::Started => self.running_sessions.push((id, name)),
                    SessionState::Exited { code, tail, error } => {
                        self.running_sessions.retain(|(i, _)| *i != id);
                        let pairing = self.pair_pending.as_deref() == Some(name.as_str());
                        let detail = logic::summarize_tail(&tail, 3);
                        match (error, code) {
                            (Some(e), _) => {
                                self.pair_pending = None;
                                self.notify(Level::Error, e)
                            }
                            (None, Some(0)) => self.notify(Level::Info, format!("Session « {name} » terminée.")),
                            // Code 4 : aucun décodeur vidéo utilisable (la connexion, elle, fonctionnait).
                            (None, Some(4)) => {
                                if pairing {
                                    self.pair_pending = None;
                                }
                                self.notify(
                                    Level::Error,
                                    format!("Session « {name} » : décodage vidéo impossible sur ce PC. {detail}"),
                                )
                            }
                            (None, code) => {
                                if pairing {
                                    self.pair_pending = None;
                                }
                                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                                self.notify(
                                    Level::Error,
                                    format!(
                                        "Connexion à « {name} » interrompue (code {code}). {detail} — Vérifiez que le host tourne, l'adresse, le pare-feu/NAT et que l'invitation n'a pas expiré."
                                    ),
                                )
                            }
                        }
                    }
                },
                Event::RunStatus(rs) => {
                    if let Some((st, _)) = &rs {
                        if let (true, Some(ext)) = (st.upnp.ok, st.upnp.external_addr.as_deref()) {
                            if logic::should_autofill(&self.settings.public_addr, self.settings.public_addr_auto, ext) {
                                self.settings.public_addr = ext.to_string();
                                self.settings.public_addr_auto = true;
                            }
                        }
                    }
                    if self.opts.auto_firewall_help && !self.firewall_help_pending && self.firewall_help.is_none() {
                        if let Some((st, _)) = &rs {
                            self.opts.auto_firewall_help = false;
                            self.firewall_help_pending = true;
                            let _ = self.host_tx.send(HostCmd::FirewallHelp { port: st.port().unwrap_or(DEFAULT_PORT) });
                        }
                    }
                    self.run_status = rs;
                }
                Event::FirewallHelp(r) => {
                    self.firewall_help_pending = false;
                    self.firewall_help = Some(r);
                }
                Event::AutoAddr(a) => {
                    self.auto_addr_pending = false;
                    let notes = a.notes.join(" ");
                    match a.addr {
                        Some(addr) => {
                            self.settings.public_addr = addr;
                            self.settings.public_addr_auto = true;
                            self.notify(Level::Success, format!("Adresse publique détectée. {notes}"));
                        }
                        None => self.notify(Level::Error, format!("Adresse publique introuvable : {notes}")),
                    }
                }
                Event::Pasted(t) => {
                    self.link_text = t;
                    self.name_touched = false;
                }
                Event::Elevation(st) => self.elevation = st,
                Event::Update(ev) => {
                    self.update.on_event(ev);
                    if matches!(self.update.state, update::UpdateState::Launched) {
                        // Le setup remplace les fichiers : on ferme (le host lancé d'ici est déjà arrêté,
                        // l'installation n'a démarré qu'une fois les sessions terminées).
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
    }

    fn on_invite_created(&mut self, ctx: &egui::Context, ic: InviteCreated) {
        self.invite_pending = false;
        self.invite_note = None;
        // Image du QR : un texel = un pixel physique pour que les modules restent nets.
        let ppp = ctx.pixels_per_point();
        let scale = (5.0 * ppp).ceil().max(3.0) as usize;
        let (texture, qr_error) = match QrMatrix::encode(&ic.uri) {
            Ok(m) => (Some(ctx.load_texture("invite-qr", m.to_color_image(scale), TextureOptions::NEAREST)), None),
            Err(e) => (None, Some(e)),
        };
        if self.opts.auto_copy_invite {
            backend::clipboard_copy(&self.sink, ic.uri.clone(), "Lien d'invitation");
        }
        self.invite = Some(ActiveInvite { uri: ic.uri.clone(), uri_v6: ic.uri_v6.clone(), addr: ic.addr.clone(), expires_ms: ic.expires_ms, texture, qr_error });
    }

    fn create_invite(&mut self) {
        match validate_addr(&self.settings.public_addr) {
            Ok(addr) => {
                self.settings.public_addr = addr.clone();
                self.invite = None;
                self.invite_pending = true;
                let alt_addr = self.run_status.as_ref().and_then(|(s, _)| s.ipv6_alt());
                let _ = self.data_tx.send(DataCmd::CreateInvite { addr, alt_addr, ttl_ms: self.settings.ttl.ms() });
                self.save_settings();
            }
            Err(m) => self.notify(Level::Error, m),
        }
    }

    fn start_host(&mut self) {
        match self.settings.host.validate() {
            Ok(_) => {
                self.save_settings();
                let args = self.settings.host.to_args(&self.paths.host_dir);
                // Mode Contrôle total : lancer le host élevé via la tâche planifiée (aucune UAC à
                // chaque démarrage), à condition qu'elle soit installée et valide.
                if self.settings.host_elevated && self.elevation.task_present && self.elevation.task_valid {
                    let _ = self.host_tx.send(HostCmd::StartElevated(args));
                } else {
                    if self.settings.host_elevated {
                        self.notify(Level::Info, "Mode Contrôle total non activé : démarrage normal (host non élevé).");
                    }
                    let _ = self.host_tx.send(HostCmd::Start(args));
                }
            }
            Err(m) => self.notify(Level::Error, m),
        }
    }

    fn launch_client(&mut self, target_line: String, args: Vec<String>, name: String) {
        self.save_settings();
        self.sessions.launch(self.paths.client_exe.clone(), args, target_line, name, self.sink.clone());
    }

    // -----------------------------------------------------------------------------------------
    // Dessin
    // -----------------------------------------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("GamingRemoteExperience").heading().strong());
            ui.label(muted("Appairage"));
            ui.add_space(18.0);
            for (tab, label) in
                [(Tab::Client, "Client — PC qui contrôle"), (Tab::Host, "Host — PC à contrôler"), (Tab::Settings, "Paramètres")]
            {
                let selected = self.tab == tab;
                let text = RichText::new(label).size(15.5);
                let btn = egui::Button::selectable(selected, if selected { text.strong() } else { text });
                if ui.add(btn).clicked() {
                    self.tab = tab;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut action = MenuAction::None;
                ui.menu_button(format!("v{}", update::current_version()), |ui| {
                    ui.set_min_width(300.0);
                    action = self.update.menu(ui, &mut self.settings.update);
                })
                .response
                .on_hover_text("Version installée et mises à jour");
                match action {
                    MenuAction::CheckNow => self.check_updates(true),
                    MenuAction::PrefsChanged => self.save_settings(),
                    MenuAction::None => {}
                }
            });
        });
    }

    fn check_updates(&mut self, manual: bool) {
        if self.update.checking() {
            return;
        }
        if !update::configured() {
            self.update.state = update::UpdateState::NotConfigured;
            return;
        }
        self.update.state = update::UpdateState::Checking;
        self.settings.update.last_check_ms = now_ms();
        self.save_settings();
        update::spawn_check(&self.sink, manual);
    }

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        match self.update.banner(ui) {
            BannerAction::Install { force_stop } => {
                let avail = match &self.update.state {
                    update::UpdateState::Available(a) | update::UpdateState::Waiting { avail: a, .. } => Some((**a).clone()),
                    _ => None,
                };
                if let Some(a) = avail {
                    if force_stop && self.host.ours && self.host.running {
                        let _ = self.host_tx.send(HostCmd::Stop);
                    }
                    update::spawn_install(&self.sink, a, &self.update, force_stop);
                }
            }
            BannerAction::Dismiss => self.update.dismissed = true,
            BannerAction::CancelWait => self.update.cancel_wait(),
            BannerAction::None => {}
        }
    }

    fn notice_bar(&mut self, ui: &mut egui::Ui) {
        let Some(n) = &self.notice else {
            ui.label(small_muted("Prêt."));
            return;
        };
        let color = match n.level {
            Level::Info => MUTED,
            Level::Success => OK,
            Level::Error => ERR,
        };
        let text = n.text.clone();
        let mut dismiss = false;
        ui.horizontal_wrapped(|ui| {
            dot(ui, color);
            ui.label(colored(text, color));
            if ui.small_button("Fermer").clicked() {
                dismiss = true;
            }
        });
        let ttl = if n.level == Level::Error { 30 } else { 10 };
        if dismiss || n.at.elapsed() > Duration::from_secs(ttl) {
            self.notice = None;
        }
    }

    fn fingerprint_row(&self, ui: &mut egui::Ui, fp: &Option<Result<String, String>>, what: &'static str) {
        match fp {
            None => {
                ui.label(muted("Chargement…"));
            }
            Some(Err(e)) => {
                ui.label(colored(e, ERR));
            }
            Some(Ok(fp)) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(fp).monospace().size(18.0).strong());
                    if ui.button("Copier").clicked() {
                        backend::clipboard_copy(&self.sink, fp.clone(), what);
                    }
                });
            }
        }
    }

    // ----- Host ---------------------------------------------------------------------------------

    fn host_tab(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let _ = now;
        ui.columns(2, |cols| {
            // Mise en page non justifiée : sinon egui justifie le texte replié (espaces étirés).
            cols[0].with_layout(Layout::top_down(Align::Min), |ui| self.identity_card(ui));
            cols[1].with_layout(Layout::top_down(Align::Min), |ui| self.status_card(ui));
        });
        self.invite_card(ui);
        self.clients_card(ui);
    }

    fn identity_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Identité de ce host", |ui| {
            ui.label(muted("Empreinte à communiquer au client pour qu'il la vérifie à l'œil :"));
            self.fingerprint_row(ui, &self.host_fp.clone(), "Empreinte du host");
            ui.label(muted("Dossier de données :"));
            let dir = self.paths.host_dir.display().to_string();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&dir).monospace().size(13.0));
                if ui.small_button("Copier").clicked() {
                    backend::clipboard_copy(&self.sink, dir.clone(), "Chemin");
                }
            });
            ui.label(small_muted("La clé privée ne quitte jamais ce dossier et n'est jamais affichée."));
        });
    }

    fn status_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Statut du host (grx-host run)", |ui| {
            let st = self.host.clone();
            ui.horizontal(|ui| {
                if st.stopping {
                    dot(ui, WARN);
                    ui.label(colored("Arrêt en cours…", WARN));
                } else if st.running {
                    dot(ui, OK);
                    let who = if st.ours { "lancé depuis cette fenêtre" } else { "lancé ailleurs (terminal…)" };
                    ui.label(colored(format!("En cours — PID {} ({who})", st.pid.unwrap_or(0)), OK));
                } else {
                    dot(ui, MUTED);
                    ui.label(muted("Arrêté"));
                }
            });
            if let Some(m) = &st.message {
                if !st.running {
                    ui.label(colored(m, WARN));
                }
            }
            ui.horizontal(|ui| {
                let can_start = !st.running && !st.stopping;
                if ui.add_enabled(can_start, primary_button("Démarrer le host")).clicked() {
                    self.start_host();
                }
                if ui.add_enabled(st.running && st.ours && !st.stopping, egui::Button::new("Arrêter")).clicked() {
                    let _ = self.host_tx.send(HostCmd::Stop);
                }
            });
            if st.running && !st.ours {
                ui.label(small_muted("Ce host n'a pas été démarré ici : fermez-le depuis son terminal (Ctrl+C)."));
            }
            if st.running && !st.stopping {
                self.access_status(ui);
            }
            self.elevation_card(ui, st.running);
            if settings_tab::forced_badges(ui, &self.settings.host.forced()) {
                self.tab = Tab::Settings;
            }
            egui::CollapsingHeader::new("Options de lancement").default_open(false).show(ui, |ui| {
                let o = &mut self.settings.host;
                let locked = st.running;
                ui.add_enabled_ui(!locked, |ui| {
                    egui::Grid::new("host-opts").num_columns(2).show(ui, |ui| {
                        ui.label("Écoute (bind)");
                        ui.add(TextEdit::singleline(&mut o.bind).desired_width(170.0).font(egui::TextStyle::Monospace));
                        ui.end_row();
                        ui.label("Codec");
                        egui::ComboBox::from_id_salt("codec").selected_text(o.codec.label()).show_ui(ui, |ui| {
                            for c in CodecChoice::ALL {
                                ui.selectable_value(&mut o.codec, c, c.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Images / seconde");
                        ui.add(egui::DragValue::new(&mut o.fps).range(1..=240).speed(0.5));
                        ui.end_row();
                    });
                    ui.checkbox(&mut o.no_input, "Aucune entrée injectée (--no-input) : test sans risque");
                    ui.add_enabled_ui(!o.no_input, |ui| {
                        ui.checkbox(&mut o.no_keyboard, "Pas de clavier (--no-keyboard)");
                        ui.checkbox(&mut o.no_mouse, "Pas de souris (--no-mouse)");
                        ui.checkbox(&mut o.no_gamepad, "Pas de manette (--no-gamepad)");
                        ui.checkbox(&mut o.block_win_key, "Bloquer la touche Windows (--block-win-key)");
                    });
                    ui.checkbox(&mut o.no_audio, "Pas d'audio (--no-audio)");
                    ui.checkbox(&mut o.upnp, "Ouvrir le port automatiquement sur la box (UPnP / PCP / NAT-PMP)");
                    ui.add_enabled_ui(o.upnp, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Port externe sur la box :");
                            ui.add(egui::DragValue::new(&mut o.external_port).range(0..=65535).speed(1.0));
                            ui.label(small_muted(if o.external_port == 0 { "(0 = même que le port d'écoute)" } else { "" }));
                        });
                    });
                    if !o.upnp {
                        ui.label(small_muted("Sans UPnP, redirigez le port UDP à la main sur la box pour un accès depuis Internet."));
                    }
                });
                if locked {
                    ui.label(small_muted("Arrêtez le host pour modifier ces options."));
                }
                if ui.link("Tous les réglages (débit, écran, manettes…) : onglet Paramètres").clicked() {
                    self.tab = Tab::Settings;
                }
            });
            ui.label(muted("Dernières lignes de sortie :"));
            egui::Frame::new()
                .fill(theme::FIELD)
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ScrollArea::vertical().max_height(96.0).id_salt("host-log").stick_to_bottom(true).show(ui, |ui| {
                        if self.host_log.is_empty() {
                            ui.label(small_muted("(aucune sortie)"));
                        }
                        let from = self.host_log.len().saturating_sub(40);
                        for l in &self.host_log[from..] {
                            ui.label(RichText::new(l).monospace().size(12.5));
                        }
                    });
                });
        });
    }

    /// Mode « Contrôle total » : host lancé élevé par une tâche planifiée (sans UAC à chaque fois).
    fn elevation_card(&mut self, ui: &mut egui::Ui, running: bool) {
        egui::CollapsingHeader::new("Contrôle total (host élevé)").default_open(false).show(ui, |ui| {
            let e = self.elevation.clone();
            ui.label(small_muted(
                "Permet à un client « Contrôle total » de piloter au clavier/souris les fenêtres administrateur du PC. \
                 L'invite UAC et l'écran de verrouillage de Windows restent protégés : jamais visibles ni pilotables à distance (à valider sur place).",
            ));
            if !e.protected_location {
                ui.label(colored(
                    "Indisponible : grx-host n'est pas installé « pour tous les utilisateurs » (Program Files). \
                     Réinstallez en installation machine pour activer le Contrôle total en toute sécurité.",
                    WARN,
                ));
                return;
            }
            ui.horizontal(|ui| {
                if e.task_present && e.task_valid {
                    dot(ui, OK);
                    ui.label(colored("Activé (tâche planifiée installée)", OK));
                } else if e.task_present {
                    dot(ui, WARN);
                    ui.label(colored("Tâche présente mais invalide (réactivez)", WARN));
                } else {
                    dot(ui, MUTED);
                    ui.label(muted("Désactivé"));
                }
            });
            ui.add_enabled_ui(!running, |ui| {
                ui.checkbox(&mut self.settings.host_elevated, "Démarrer le host en mode Contrôle total");
                ui.checkbox(&mut self.settings.host_at_logon, "Lancer le host élevé à l'ouverture de session (optionnel)");
                ui.horizontal(|ui| {
                    if !e.task_present {
                        if ui.add(primary_button("Activer (UAC)")).clicked() {
                            let _ = self.host_tx.send(HostCmd::ElevationEnable { at_logon: self.settings.host_at_logon });
                            self.save_settings();
                        }
                    } else {
                        if ui.button("Réappliquer (UAC)").clicked() {
                            let _ = self.host_tx.send(HostCmd::ElevationEnable { at_logon: self.settings.host_at_logon });
                            self.save_settings();
                        }
                        if ui.add(danger_button("Désactiver (UAC)")).clicked() {
                            let _ = self.host_tx.send(HostCmd::ElevationDisable);
                        }
                    }
                });
            });
            if running {
                ui.label(small_muted("Arrêtez le host pour changer ces réglages."));
            }
        });
    }

    /// Redirection de port et pare-feu, d'après `status.json` du host vivant.
    fn access_status(&mut self, ui: &mut egui::Ui) {
        let Some((rs, age)) = self.run_status.clone() else {
            ui.label(small_muted("Accès Internet : état détaillé indisponible (status.json absent ; host trop ancien ?)."));
            return;
        };
        let tone_color = |t: &Tone| match t {
            Tone::Ok => OK,
            Tone::Pending => MUTED,
            Tone::Warn => WARN,
            Tone::Error => ERR,
        };
        let show = |ui: &mut egui::Ui, l: &hoststatus::Line| {
            ui.horizontal_wrapped(|ui| {
                dot(ui, tone_color(&l.tone));
                ui.label(colored(&l.title, tone_color(&l.tone)));
            });
            if let Some(d) = &l.detail {
                ui.label(small_muted(d));
            }
            if let Some(a) = &l.advice {
                ui.label(RichText::new(a).small());
            }
        };
        ui.separator();
        show(ui, &hoststatus::redirect_line(&rs, age));
        let fw = hoststatus::firewall_line(&rs);
        show(ui, &fw);
        ui.horizontal(|ui| {
            let port = rs.port().unwrap_or(DEFAULT_PORT);
            if ui
                .add_enabled(!self.firewall_help_pending, egui::Button::new("Voir la commande (pare-feu)"))
                .on_hover_text("Affiche la commande à lancer vous-même en administrateur. L'interface ne l'exécute jamais.")
                .clicked()
            {
                self.firewall_help_pending = true;
                let _ = self.host_tx.send(HostCmd::FirewallHelp { port });
            }
            if rs.dual_stack() && !rs.ipv6.is_empty() {
                ui.label(small_muted("IPv6 active : un lien IPv6 sera proposé avec l'invitation."));
            }
        });
    }

    fn invite_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Inviter un nouveau client", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Adresse publique du host :");
                let r = ui.add(
                    TextEdit::singleline(&mut self.settings.public_addr)
                        .hint_text("ip-ou-dns:7777")
                        .desired_width(260.0)
                        .font(egui::TextStyle::Monospace),
                );
                if r.changed() {
                    self.settings.public_addr_auto = false;
                }
                let auto = ui
                    .add_enabled(!self.auto_addr_pending, primary_button("Adresse publique automatique"))
                    .on_hover_text("Adresse de la redirection ouverte par le host, sinon IP externe demandée à votre box (réseau local uniquement, aucun service Internet).");
                if auto.clicked() {
                    let port = self.settings.host.bind.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(DEFAULT_PORT);
                    let port = if self.settings.host.upnp && self.settings.host.external_port != 0 { self.settings.host.external_port } else { port };
                    self.auto_addr_pending = true;
                    let _ = self.data_tx.send(DataCmd::AutoAddr { port });
                }
                let resp = ui
                    .add_enabled(!self.detect_ip_pending, egui::Button::new("Détecter mon IP locale"))
                    .on_hover_text("Lit l'adresse de votre carte réseau. Aucune requête n'est envoyée sur Internet (pour une IP publique, utilisez votre DNS ou votre VPN).");
                if resp.clicked() {
                    let port = self
                        .settings
                        .public_addr
                        .rsplit_once(':')
                        .and_then(|(_, p)| p.parse::<u16>().ok())
                        .unwrap_or(DEFAULT_PORT);
                    self.detect_ip_pending = true;
                    self.settings.public_addr_auto = false;
                    let _ = self.data_tx.send(DataCmd::DetectLocalIp { port });
                }
            });
            if self.auto_addr_pending {
                ui.label(muted("Interrogation de la box…"));
            } else if self.settings.public_addr_auto && !self.settings.public_addr.is_empty() {
                ui.label(small_muted("Adresse remplie automatiquement (redirection du host / IP externe de la box)."));
            }
            let addr_check = validate_addr(&self.settings.public_addr);
            if let (Err(m), false) = (&addr_check, self.settings.public_addr.trim().is_empty()) {
                ui.label(colored(m, ERR));
            } else if self.settings.public_addr.trim().is_empty() {
                ui.label(small_muted("Adresse que le client utilisera pour joindre ce PC (IP/DNS + port UDP redirigé si Internet)."));
            }
            ui.horizontal(|ui| {
                ui.label("Valable :");
                for t in Ttl::ALL {
                    ui.selectable_value(&mut self.settings.ttl, t, t.label());
                }
                ui.add_space(12.0);
                let enabled = addr_check.is_ok() && !self.invite_pending;
                if ui.add_enabled(enabled, primary_button("Créer l'invitation")).clicked() {
                    self.create_invite();
                }
            });
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(58, 46, 20))
                .stroke(egui::Stroke::new(1.0, WARN))
                .corner_radius(egui::CornerRadius::same(7))
                .inner_margin(egui::Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(colored(
                        "Attention : ce lien contient un secret à usage unique. Quiconque l'obtient avant le client peut s'ajouter à la liste des clients autorisés. Transmettez-le par un canal sûr (en main propre, messagerie chiffrée).",
                        WARN,
                    ));
                });

            if let Some(note) = &self.invite_note {
                ui.label(colored(note, OK));
            }
            let mut clear = false;
            let mut expired = false;
            if let Some(inv) = &self.invite {
                let secs = remaining_secs(inv.expires_ms, now_ms());
                if secs == 0 {
                    expired = true;
                } else {
                    ui.separator();
                    ui.horizontal_top(|ui| {
                        if let Some(tex) = &inv.texture {
                            let side = tex.size_vec2() / ui.ctx().pixels_per_point();
                            egui::Frame::new().fill(egui::Color32::WHITE).corner_radius(egui::CornerRadius::same(6)).show(ui, |ui| {
                                ui.add(egui::Image::new((tex.id(), side)));
                            });
                        } else if let Some(e) = &inv.qr_error {
                            ui.label(colored(e, ERR));
                        }
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(colored(format!("Expire dans {}", format_countdown(secs)), if secs < 60 { WARN } else { OK }));
                                ui.label(small_muted(format!("— adresse : {}", inv.addr)));
                            });
                            let mut shown = inv.uri.clone();
                            ui.add(
                                TextEdit::multiline(&mut shown)
                                    .desired_rows(4)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace)
                                    .interactive(true),
                            );
                            shown.zeroize();
                            ui.horizontal(|ui| {
                                if ui.add(primary_button("Copier le lien")).clicked() {
                                    backend::clipboard_copy(&self.sink, inv.uri.clone(), "Lien d'invitation");
                                }
                                if ui.button("Effacer l'invitation").on_hover_text("Efface le lien de l'écran (le jeton reste valable jusqu'à son expiration côté host).").clicked() {
                                    clear = true;
                                }
                            });
                            ui.label(small_muted(
                                "Le client scanne le QR code ou colle le lien dans l'onglet Client. Le presse-papiers garde le lien jusqu'à son remplacement.",
                            ));
                            if let Some(v6) = &inv.uri_v6 {
                                ui.add_space(4.0);
                                ui.label(muted("Alternative IPv6 (même jeton : un seul des deux liens servira) :"));
                                let mut shown = v6.clone();
                                ui.add(
                                    TextEdit::multiline(&mut shown)
                                        .desired_rows(2)
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace),
                                );
                                shown.zeroize();
                                if ui.button("Copier le lien IPv6").clicked() {
                                    backend::clipboard_copy(&self.sink, v6.clone(), "Lien IPv6");
                                }
                            }
                        });
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
            }
            if clear {
                self.invite = None;
            }
            if expired {
                self.invite = None;
                self.invite_note = Some("Invitation expirée : créez-en une nouvelle.".into());
            }
            if self.invite_pending {
                ui.label(muted("Création en cours…"));
            }
        });
    }

    fn clients_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Clients autorisés", |ui| {
            if let Some(e) = &self.clients_err {
                ui.label(colored(e, ERR));
            }
            if self.clients.rows.is_empty() {
                let msg = if self.clients.is_loaded() {
                    "Aucun client autorisé pour l'instant. Créez une invitation : le client apparaîtra ici tout seul dès qu'il l'aura utilisée."
                } else {
                    "Chargement…"
                };
                ui.label(muted(msg));
            } else {
                let now = Instant::now();
                let rows = self.clients.rows.clone();
                let mut set_role: Option<(ClientRow, grx_core::Role)> = None;
                let mut revoke: Option<ClientRow> = None;
                let mut highlight = false;
                egui::Grid::new("clients").striped(true).num_columns(5).min_col_width(60.0).show(ui, |ui| {
                    for h in ["Nom", "Empreinte", "Ajouté le", "Rôle", ""] {
                        ui.label(small_muted(h));
                    }
                    ui.end_row();
                    for r in &rows {
                        let is_new = self.clients.is_new(&r.fingerprint, now);
                        highlight |= is_new;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&r.name).strong());
                            if is_new {
                                ui.label(colored("nouveau", OK).small());
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&r.fingerprint).monospace().size(13.0));
                            if ui.small_button("Copier").clicked() {
                                backend::clipboard_copy(&self.sink, r.fingerprint.clone(), "Empreinte");
                            }
                        });
                        ui.label(format_date_local(r.added_ms));
                        let mut role = r.role;
                        egui::ComboBox::from_id_salt(("role", &r.fingerprint))
                            .selected_text(r.role_label())
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut role, grx_core::Role::Full, "Complet");
                                ui.selectable_value(&mut role, grx_core::Role::ViewOnly, "Lecture seule");
                                ui.selectable_value(&mut role, grx_core::Role::Total, "Contrôle total");
                            });
                        if role != r.role {
                            // « Contrôle total » demande une confirmation explicite (il pourra
                            // piloter les fenêtres administrateur du PC quand le host est élevé).
                            if role == grx_core::Role::Total {
                                self.total_confirm = Some(r.clone());
                            } else {
                                set_role = Some((r.clone(), role));
                            }
                        }
                        if ui.add(danger_button("Révoquer")).clicked() {
                            revoke = Some(r.clone());
                        }
                        ui.end_row();
                    }
                });
                if highlight {
                    ui.ctx().request_repaint_after(Duration::from_millis(1000));
                }
                if let Some((r, role)) = set_role {
                    let _ = self.data_tx.send(DataCmd::SetRole { fingerprint: r.fingerprint, name: r.name, role });
                }
                if let Some(r) = revoke {
                    self.revoke_confirm = Some(r);
                }
                ui.label(small_muted(
                    "Complet : clavier, souris, manette sur le bureau normal. Lecture seule : le client voit et entend, rien n'est injecté. \
                     Contrôle total : comme Complet, et pilote aussi les fenêtres administrateur quand le host tourne en mode Contrôle total. \
                     Changer le rôle ou révoquer coupe la session en cours. La liste se met à jour toute seule.",
                ));
            }
        });
    }

    // ----- Client -------------------------------------------------------------------------------

    fn client_tab(&mut self, ui: &mut egui::Ui) {
        card(ui, "Cet ordinateur (client)", |ui| {
            ui.label(muted("Empreinte de ce client : communiquez-la au propriétaire du host (il peut la retrouver dans sa liste de clients autorisés) :"));
            self.fingerprint_row(ui, &self.client_fp.clone(), "Empreinte du client");
            ui.label(small_muted(format!("Dossier de données : {}", self.paths.client_dir.display())));
        });
        self.hosts_card(ui);
        self.add_host_card(ui);
    }

    fn hosts_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Hosts mémorisés", |ui| {
            if !self.running_sessions.is_empty() {
                let names: Vec<_> = self.running_sessions.iter().map(|(_, n)| format!("« {n} »")).collect();
                ui.horizontal(|ui| {
                    dot(ui, OK);
                    ui.label(colored(format!("Session en cours : {}", names.join(", ")), OK));
                });
            }
            if let Some(e) = &self.profiles_err {
                ui.label(colored(e, ERR));
            }
            if self.profiles.is_empty() {
                let msg = if self.profiles_loaded {
                    "Aucun host mémorisé. Collez un lien d'invitation ci-dessous pour en ajouter un."
                } else {
                    "Chargement…"
                };
                ui.label(muted(msg));
            }
            let profiles = self.profiles.clone();
            let mut connect: Option<ProfileRow> = None;
            let mut start_rename: Option<ProfileRow> = None;
            let mut delete: Option<ProfileRow> = None;
            if !profiles.is_empty() {
                egui::Grid::new("hosts").striped(true).num_columns(4).min_col_width(60.0).show(ui, |ui| {
                    for h in ["Nom", "Adresse", "Empreinte du host", ""] {
                        ui.label(small_muted(h));
                    }
                    ui.end_row();
                    for p in &profiles {
                        ui.label(RichText::new(&p.name).strong());
                        ui.label(RichText::new(&p.addr).monospace().size(13.5));
                        ui.label(RichText::new(&p.fingerprint).monospace().size(13.0));
                        ui.horizontal(|ui| {
                            if ui.add(primary_button("Connecter")).clicked() {
                                connect = Some(p.clone());
                            }
                            if ui.button("Renommer").clicked() {
                                start_rename = Some(p.clone());
                            }
                            if ui.add(danger_button("Supprimer")).clicked() {
                                delete = Some(p.clone());
                            }
                        });
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(muted("Au lancement :"));
                    ui.checkbox(&mut self.settings.client.no_input, "sans envoyer d'entrées (--no-input)");
                    ui.checkbox(&mut self.settings.client.no_audio, "sans audio (--no-audio)");
                    ui.checkbox(&mut self.settings.client.fullscreen, "plein écran (--fullscreen)");
                    if ui.link("autres réglages…").on_hover_text("Onglet Paramètres").clicked() {
                        self.tab = Tab::Settings;
                    }
                });
                let ctx = ui.ctx().clone();
                if let Some(w) = self.client_codec_warning(&ctx) {
                    ui.label(colored(format!("⚠ {w}"), WARN));
                }
                if settings_tab::forced_badges(ui, &self.settings.client.forced()) {
                    self.tab = Tab::Settings;
                }
            }
            if let Some(p) = connect {
                let args = self.settings.client.to_args(&self.paths.client_dir, "-", None);
                self.notify(Level::Info, format!("Connexion à « {} »…", p.name));
                self.launch_client(p.name.clone(), args, p.name);
            }
            if let Some(p) = start_rename {
                self.rename = Some((p.name.clone(), p.name));
            }
            if let Some(p) = delete {
                self.delete_confirm = Some(p.name);
            }
            if let Some((old, buf)) = &mut self.rename {
                let existing: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).filter(|n| n != old).collect();
                let check = validate_name(buf, &existing);
                let mut done = None;
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(format!("Renommer « {old} » en :"));
                    let r = ui.add(TextEdit::singleline(buf).desired_width(220.0));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add_enabled(check.is_ok(), primary_button("Valider")).clicked() || (enter && check.is_ok()) {
                        done = Some(true);
                    }
                    if ui.button("Annuler").clicked() {
                        done = Some(false);
                    }
                });
                if let Err(m) = &check {
                    ui.label(colored(m, ERR));
                }
                if let Some(ok) = done {
                    if let (true, Ok(new)) = (ok, check) {
                        if &new != old {
                            let _ = self.data_tx.send(DataCmd::RenameHost { old: old.clone(), new });
                        }
                    }
                    self.rename = None;
                }
            }
        });
    }

    fn add_host_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Ajouter un host (première connexion)", |ui| {
            ui.label(muted("Collez le lien grx:// reçu du propriétaire du host :"));
            let r = ui.add(
                TextEdit::multiline(&mut self.link_text)
                    .hint_text("grx://adresse:7777/clé-du-host/jeton")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
            if r.changed() {
                self.name_touched = false;
            }
            ui.horizontal(|ui| {
                if ui.button("Coller depuis le presse-papiers").clicked() {
                    backend::clipboard_paste(&self.sink);
                }
                if ui.add_enabled(!self.link_text.is_empty(), egui::Button::new("Effacer")).clicked() {
                    self.link_text.zeroize();
                    self.link_text.clear();
                    self.name_touched = false;
                }
            });
            let check = validate_link(&self.link_text);
            let existing: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
            let mut can_pair = false;
            match &check {
                LinkCheck::Empty => {
                    ui.label(small_muted("Le lien est analysé dès qu'il est collé ; rien n'est envoyé tant que vous n'avez pas cliqué sur « Appairer et connecter »."));
                }
                LinkCheck::Invalid(m) => {
                    ui.horizontal_wrapped(|ui| {
                        dot(ui, ERR);
                        ui.label(colored(m, ERR));
                    });
                }
                LinkCheck::Valid(info) => {
                    can_pair = true;
                    ui.horizontal(|ui| {
                        dot(ui, OK);
                        ui.label(colored("Lien valide", OK));
                    });
                    egui::Grid::new("link-info").num_columns(2).show(ui, |ui| {
                        ui.label(muted("Adresse du host"));
                        ui.label(RichText::new(&info.addr).monospace().size(15.0).strong());
                        ui.end_row();
                        ui.label(muted("Empreinte du host"));
                        ui.label(RichText::new(&info.fingerprint).monospace().size(17.0).strong());
                        ui.end_row();
                    });
                    ui.label(colored(
                        "Comparez cette empreinte avec celle affichée sur le PC host (onglet Host) avant de continuer : c'est ce qui vous protège d'un faux host.",
                        WARN,
                    ));
                    if !info.has_token {
                        ui.label(colored(
                            "Ce lien ne contient pas de jeton : la connexion ne marchera que si le host vous a déjà autorisé.",
                            MUTED,
                        ));
                    }
                    if !self.name_touched {
                        self.name_text = logic::suggest_name(&info.addr, &existing);
                    }
                }
            }
            if can_pair {
                if !self.last_link_valid {
                    self.scroll_to_pair = 6;
                }
                let busy = self.pair_pending.is_some();
                let mut name_check = validate_name(&self.name_text, &existing);
                // Nom et bouton sur la même ligne : visible sans défiler à la taille par défaut.
                let pair_btn = ui
                    .horizontal(|ui| {
                        ui.label("Nom du host :");
                        let r = ui.add(TextEdit::singleline(&mut self.name_text).desired_width(240.0));
                        if r.changed() {
                            self.name_touched = true;
                            name_check = validate_name(&self.name_text, &existing);
                        }
                        ui.add_space(8.0);
                        ui.add_enabled(name_check.is_ok() && !busy, primary_button("Appairer et connecter"))
                    })
                    .inner;
                if self.scroll_to_pair > 0 {
                    self.scroll_to_pair -= 1;
                    pair_btn.scroll_to_me(Some(Align::BOTTOM));
                    ui.ctx().request_repaint();
                }
                if let Err(m) = &name_check {
                    ui.label(colored(m, ERR));
                }
                if pair_btn
                    .on_hover_text("Lance grx-client : première connexion avec l'invitation, puis le host est mémorisé sous ce nom.")
                    .clicked()
                {
                    if let (LinkCheck::Valid(info), Ok(name)) = (&check, name_check) {
                        let args = self.settings.client.to_args(&self.paths.client_dir, "-", Some(&name));
                        self.pair_pending = Some(name.clone());
                        self.notify(Level::Info, format!("Appairage avec « {name} » ({})…", info.addr));
                        self.launch_client(info.uri.clone(), args, name);
                    }
                }
                if busy {
                    ui.label(muted("Appairage en cours : une fenêtre de streaming s'ouvre dès que la connexion est établie."));
                }
            }
            self.last_link_valid = can_pair;
            ui.label(small_muted(
                "Scan de QR code : non disponible dans cette version (la caméra n'est pas utilisée) ; scannez le QR avec un téléphone et collez le lien ici.",
            ));
        });
    }

    // ----- Dialogues ---------------------------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(r) = self.revoke_confirm.clone() {
            let mut close = false;
            let mut confirm = false;
            let m = egui::Modal::new(egui::Id::unique("revoke-modal")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading("Révoquer ce client ?");
                ui.label(format!("« {} » ({}) ne pourra plus se connecter, et sa session en cours sera coupée.", r.name, r.fingerprint));
                ui.label(muted("Pour lui redonner l'accès, il faudra créer une nouvelle invitation."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(danger_button("Révoquer")).clicked() {
                        confirm = true;
                    }
                    if ui.button("Annuler").clicked() {
                        close = true;
                    }
                });
            });
            if m.should_close() {
                close = true;
            }
            if confirm {
                let _ = self.data_tx.send(DataCmd::Revoke { fingerprint: r.fingerprint, name: r.name });
                self.revoke_confirm = None;
            } else if close {
                self.revoke_confirm = None;
            }
        }
        if let Some(r) = self.total_confirm.clone() {
            let mut close = false;
            let mut confirm = false;
            let m = egui::Modal::new(egui::Id::unique("total-modal")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading("Donner le « Contrôle total » ?");
                ui.label(format!(
                    "« {} » ({}) pourra piloter au clavier et à la souris les fenêtres administrateur du PC \
                     (ex. valider certaines boîtes de dialogue) quand le host tourne en mode Contrôle total.",
                    r.name, r.fingerprint
                ));
                ui.label(colored(
                    "L'invite UAC de Windows et l'écran de verrouillage restent protégés : jamais visibles ni pilotables à distance.",
                    WARN,
                ));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(primary_button("Donner le Contrôle total")).clicked() {
                        confirm = true;
                    }
                    if ui.button("Annuler").clicked() {
                        close = true;
                    }
                });
            });
            if m.should_close() {
                close = true;
            }
            if confirm {
                let _ = self.data_tx.send(DataCmd::SetRole { fingerprint: r.fingerprint, name: r.name, role: grx_core::Role::Total });
                self.total_confirm = None;
            } else if close {
                self.total_confirm = None;
            }
        }
        if let Some(name) = self.delete_confirm.clone() {
            let mut close = false;
            let mut confirm = false;
            let m = egui::Modal::new(egui::Id::unique("delete-modal")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading("Supprimer ce host ?");
                ui.label(format!("« {name} » sera retiré de la liste. Il faudra une nouvelle invitation pour le reconnecter si le host vous a révoqué."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(danger_button("Supprimer")).clicked() {
                        confirm = true;
                    }
                    if ui.button("Annuler").clicked() {
                        close = true;
                    }
                });
            });
            if m.should_close() {
                close = true;
            }
            if confirm {
                let _ = self.data_tx.send(DataCmd::DeleteHost(name));
                self.delete_confirm = None;
            } else if close {
                self.delete_confirm = None;
            }
        }
        if let Some(r) = self.firewall_help.clone() {
            let mut close = false;
            let m = egui::Modal::new(egui::Id::unique("firewall-modal")).show(ctx, |ui| {
                ui.set_width(760.0);
                ui.heading("Autoriser grx-host dans le pare-feu Windows");
                ui.label(colored(
                    "Cette interface n'exécute JAMAIS ces commandes : lisez-les, puis lancez-les vous-même dans un PowerShell ouvert en administrateur.",
                    WARN,
                ));
                match &r {
                    Ok(text) => {
                        let mut shown = text.clone();
                        ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut shown)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                        ui.horizontal(|ui| {
                            if ui.add(primary_button("Copier")).clicked() {
                                backend::clipboard_copy(&self.sink, text.clone(), "Commande pare-feu");
                            }
                            if ui.button("Fermer").clicked() {
                                close = true;
                            }
                        });
                    }
                    Err(e) => {
                        ui.label(colored(e, ERR));
                        if ui.button("Fermer").clicked() {
                            close = true;
                        }
                    }
                }
            });
            if m.should_close() || close {
                self.firewall_help = None;
            }
        }
        if self.quit_confirm {
            let mut close = false;
            let mut confirm = false;
            let m = egui::Modal::new(egui::Id::unique("quit-modal")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading("Quitter ?");
                let mut what = Vec::new();
                if self.host.ours && self.host.running {
                    what.push("le host lancé depuis cette fenêtre sera arrêté proprement".to_string());
                }
                let n = self.sessions.active.load(Ordering::SeqCst);
                if n > 0 {
                    what.push(format!("{n} session(s) client lancée(s) d'ici seront fermées"));
                }
                ui.label(format!("En quittant : {}.", what.join(" ; ")));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(primary_button("Quitter")).clicked() {
                        confirm = true;
                    }
                    if ui.button("Rester").clicked() {
                        close = true;
                    }
                });
            });
            if m.should_close() {
                close = true;
            }
            if confirm {
                self.allow_close = true;
                self.quit_confirm = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if close {
                self.quit_confirm = false;
            }
        }
    }

    fn run_auto_actions(&mut self) {
        if self.auto_done {
            return;
        }
        self.auto_done = true;
        // Vérification automatique : au plus une fois par jour, désactivable, et jamais si les
        // mises à jour ne sont pas configurées (aucune requête réseau dans ces cas).
        if !self.opts.opened_from_link && update::auto_check_due(&self.settings.update, now_ms()) && update::configured() {
            self.check_updates(false);
        }
        if self.opts.auto_invite {
            self.create_invite();
        }
        if self.opts.auto_start_host {
            self.start_host();
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        self.run_auto_actions();
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            let busy = (self.host.ours && self.host.running) || self.sessions.active.load(Ordering::SeqCst) > 0;
            if busy && !self.opts.quit_without_confirm {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit_confirm = true;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("header").frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(14, 10))).show(ui, |ui| {
            self.header(ui);
        });
        egui::Panel::bottom("notices")
            .frame(egui::Frame::new().fill(theme::CARD).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                self.notice_bar(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(14, 6)))
            .show(ui, |ui| {
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        self.update_banner(ui);
                        match self.tab {
                            Tab::Host => self.host_tab(ui),
                            Tab::Client => self.client_tab(ui),
                            Tab::Settings => self.settings_tab(ui),
                        }
                        ui.add_space(6.0);
                    });
                });
            });
        self.dialogs(&ctx);
        let _ = vec2(0.0, 0.0);
        if self.notice.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings();
        // Arrête proprement le host lancé d'ici (sa sortie est un tube : il ne doit pas survivre à la fenêtre).
        self.sessions.kill_all.store(true, Ordering::SeqCst);
        let (ack_tx, ack_rx) = channel();
        if self.host_tx.send(HostCmd::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(10));
        }
        let t0 = Instant::now();
        while self.sessions.active.load(Ordering::SeqCst) > 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(150)); // laisse partir l'enregistrement des préférences
    }
}
