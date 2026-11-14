//! Travaux d'arrière-plan : toute E/S disque et tout processus vivent ici, jamais sur le thread UI.
//!
//! - `data_worker` : possède `TrustStore` (liste des clients, verrou de fichier) et `ProfileStore`
//!   (hosts.json), surveille leurs fichiers et pousse les changements à l'UI.
//! - `host_manager` : démarre / arrête `grx-host run`, détecte un host lancé ailleurs, relaie sa sortie.
//! - `launch_client` : lance `grx-client play` (un fil par session).
//!
//! L'UI communique par canaux (`DataCmd`, `HostCmd` -> `Event`) ; chaque événement réveille l'UI.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use grx_core::util::{b32_decode, default_data_dir, now_ms};
use grx_core::{fingerprint, Error as CoreError, Identity, Invite, ProfileStore, PublicKey, TrustStore};
use zeroize::Zeroize;

use crate::hoststatus::{self, RunStatus};
use crate::logic::{sanitize_display, ClientRow, LogBuffer};
use crate::settings::{self, GuiSettings};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------------------------------------
// Chemins
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct Paths {
    pub host_dir: PathBuf,
    pub client_dir: PathBuf,
    pub host_exe: PathBuf,
    pub client_exe: PathBuf,
}

impl Paths {
    pub fn resolve(host_dir: Option<PathBuf>, client_dir: Option<PathBuf>) -> Self {
        Self {
            host_dir: host_dir.unwrap_or_else(|| default_data_dir("host")),
            client_dir: client_dir.unwrap_or_else(|| default_data_dir("client")),
            host_exe: sibling_exe("grx-host"),
            client_exe: sibling_exe("grx-client"),
        }
    }
}

/// `grx-host.exe` / `grx-client.exe` à côté de `grx-gui.exe` ; à défaut, le nom nu (recherche dans le PATH).
fn sibling_exe(name: &str) -> PathBuf {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&file)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(file))
}

// ---------------------------------------------------------------------------------------------
// Événements vers l'UI
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Success,
    Error,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProfileRow {
    pub name: String,
    pub addr: String,
    pub fingerprint: String,
}

/// Invitation fraîchement créée. Le lien contient le jeton en clair : il n'existe qu'en mémoire.
pub struct InviteCreated {
    pub uri: String,
    /// Même jeton, adresse IPv6 (un seul des deux liens servira).
    pub uri_v6: Option<String>,
    pub addr: String,
    pub expires_ms: u64,
}

impl Drop for InviteCreated {
    fn drop(&mut self) {
        self.uri.zeroize();
        if let Some(u) = self.uri_v6.as_mut() {
            u.zeroize();
        }
    }
}

/// Résultat de « Adresse publique automatique ».
pub struct AutoAddr {
    pub addr: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HostStatus {
    pub running: bool,
    pub pid: Option<u32>,
    /// Lancé par cette interface (donc arrêtable proprement par elle).
    pub ours: bool,
    pub stopping: bool,
    /// Dernier événement notable (arrêt, échec de démarrage).
    pub message: Option<String>,
}

pub enum SessionState {
    Started,
    Exited { code: Option<i32>, tail: Vec<String>, error: Option<String> },
}

pub enum Event {
    HostIdentity(Result<String, String>),
    ClientIdentity(Result<String, String>),
    Clients(Result<Vec<ClientRow>, String>),
    Profiles(Result<Vec<ProfileRow>, String>),
    InviteCreated(InviteCreated),
    Notice(Level, String),
    LocalIp(String),
    Host(HostStatus),
    HostLog(Vec<String>),
    Session { id: u64, name: String, state: SessionState },
    Pasted(String),
    /// État lu dans `status.json` du host vivant (`None` = arrêté / pas de fichier) et âge du fichier.
    RunStatus(Option<(RunStatus, Duration)>),
    FirewallHelp(Result<String, String>),
    AutoAddr(AutoAddr),
    Update(crate::update::UpdateEvent),
    /// État du mode Contrôle total (tâche planifiée).
    Elevation(ElevationState),
}

#[derive(Clone)]
pub struct Sink {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Sink {
    pub fn new(ctx: egui::Context) -> (Self, Receiver<Event>) {
        let (tx, rx) = channel();
        (Self { tx, ctx }, rx)
    }

    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    pub fn notice(&self, level: Level, text: impl Into<String>) {
        self.send(Event::Notice(level, text.into()));
    }
}

// ---------------------------------------------------------------------------------------------
// Données : liste de confiance et hosts mémorisés
// ---------------------------------------------------------------------------------------------

pub enum DataCmd {
    CreateInvite { addr: String, alt_addr: Option<String>, ttl_ms: u64 },
    /// Même logique que `grx-host invite --addr auto` (status.json, sinon IP externe lue sur la box).
    AutoAddr { port: u16 },
    SetRole { fingerprint: String, name: String, role: grx_core::Role },
    Revoke { fingerprint: String, name: String },
    RenameHost { old: String, new: String },
    DeleteHost(String),
    DetectLocalIp { port: u16 },
    SaveSettings(GuiSettings),
}

type FileStamp = Option<(SystemTime, u64)>;

fn stamp_of(path: &Path) -> FileStamp {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

fn core_err(context: &str, e: &CoreError) -> String {
    match e {
        CoreError::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            format!("{context} : le fichier est verrouillé par un autre processus, réessayez dans un instant.")
        }
        CoreError::Io(io) => format!("{context} : erreur de fichier ({io})."),
        CoreError::Json(_) => format!("{context} : le fichier est corrompu (JSON invalide). Il n'a pas été modifié."),
        other => format!("{context} : {other}"),
    }
}

fn client_rows(t: &TrustStore) -> Vec<ClientRow> {
    t.clients()
        .iter()
        .map(|c| ClientRow {
            name: sanitize_display(&c.name, 60),
            fingerprint: b32_decode(&c.key)
                .and_then(|v| <PublicKey>::try_from(v).ok())
                .map(|k| fingerprint(&k))
                .unwrap_or_else(|| "?".into()),
            added_ms: c.added_ms,
            role: c.role(),
        })
        .collect()
}

/// Écrit `host-options.json` pour le host élevé (options de `grx-host run`, hors `run`), après
/// filtrage par la liste blanche de `grx_core::elevation`. Refuse en silence toute option interdite.
fn write_host_options(dir: &Path, run_args: &[String]) -> Result<(), String> {
    // `run_args` = sortie de HostRunOptions::to_args : [--data-dir, <dir>, run, …]. On ne garde
    // que ce qui suit `run`, et jamais `--data-dir` (le host élevé utilise son dossier par défaut).
    let after_run = run_args.iter().position(|a| a == "run").map(|i| i + 1).unwrap_or(0);
    let mut args: Vec<String> = Vec::new();
    let mut it = run_args[after_run..].iter();
    while let Some(a) = it.next() {
        if a == "--data-dir" {
            it.next();
            continue;
        }
        args.push(a.clone());
    }
    grx_core::elevation::validate_run_args(&args).map_err(|e| e)?;
    let o = grx_core::elevation::TaskOptions { args };
    let json = serde_json::to_vec_pretty(&o).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).ok();
    std::fs::write(dir.join(grx_core::elevation::OPTIONS_FILE), json).map_err(|e| e.to_string())
}

/// Analyse la sortie de `grx-host elevation status` (lignes « libellé : oui/non »).
fn parse_elevation_status(stdout: &str) -> ElevationState {
    let val = |needle: &str| {
        stdout
            .lines()
            .find(|l| l.contains(needle))
            .and_then(|l| l.rsplit(':').next())
            .map(|v| v.trim().eq_ignore_ascii_case("oui"))
            .unwrap_or(false)
    };
    ElevationState {
        task_present: val("Tâche planifiée présente"),
        task_valid: val("Tâche valide"),
        at_logon: val("ouverture"),
        protected_location: val("Dossier protégé"),
    }
}

fn profile_rows(s: &ProfileStore) -> Vec<ProfileRow> {
    s.hosts()
        .iter()
        .map(|p| ProfileRow {
            name: p.name.clone(),
            addr: p.addr.clone(),
            fingerprint: p.fingerprint().unwrap_or_else(|| "clé corrompue".into()),
        })
        .collect()
}

/// Adresse IPv4/IPv6 locale utilisée pour sortir vers le réseau. `connect` sur un socket UDP ne
/// transmet AUCUN paquet : il sélectionne seulement la route (adresse de documentation 192.0.2.1,
/// jamais routée sur Internet), donc aucune requête vers un service externe.
pub fn detect_local_ip() -> Result<String, String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.connect("192.0.2.1:9").map_err(|e| e.to_string())?;
    let ip = sock.local_addr().map_err(|e| e.to_string())?.ip();
    if ip.is_unspecified() || ip.is_loopback() {
        return Err("aucune interface réseau active trouvée".into());
    }
    Ok(ip.to_string())
}

pub fn spawn_data_worker(paths: Paths, sink: Sink, rx: Receiver<DataCmd>) {
    std::thread::Builder::new()
        .name("grx-data".into())
        .spawn(move || data_worker(paths, sink, rx))
        .expect("fil données");
}

fn data_worker(paths: Paths, sink: Sink, rx: Receiver<DataCmd>) {
    // Identités (la clé privée est lue puis effacée de la mémoire par Identity ; seule la clé publique reste).
    let host_pub: Option<PublicKey> = match Identity::load_or_create(&paths.host_dir.join("identity.json")) {
        Ok(id) => {
            sink.send(Event::HostIdentity(Ok(id.fingerprint())));
            Some(id.public())
        }
        Err(e) => {
            sink.send(Event::HostIdentity(Err(core_err("Identité du host illisible", &e))));
            None
        }
    };
    match Identity::load_or_create(&paths.client_dir.join("identity.json")) {
        Ok(id) => sink.send(Event::ClientIdentity(Ok(id.fingerprint()))),
        Err(e) => sink.send(Event::ClientIdentity(Err(core_err("Identité du client illisible", &e)))),
    }

    let trust_path = paths.host_dir.join("trust.json");
    let hosts_path = paths.client_dir.join(ProfileStore::FILE);
    let settings_path = settings::settings_path(&paths.host_dir);

    let mut trust: Option<TrustStore> = None;
    let mut last_rows: Option<Vec<ClientRow>> = None;
    let mut last_trust_err: Option<String> = None;
    let mut profiles: Option<ProfileStore> = None;
    let mut last_hosts_stamp: Option<FileStamp> = None;

    // Charge (ou recharge) la liste de confiance et la pousse à l'UI si elle a changé.
    let poll_trust = |trust: &mut Option<TrustStore>,
                      last_rows: &mut Option<Vec<ClientRow>>,
                      last_err: &mut Option<String>,
                      sink: &Sink| {
        let result: Result<(), CoreError> = match trust {
            Some(t) => t.refresh().map(|_| ()),
            None => TrustStore::load(&trust_path).map(|t| *trust = Some(t)),
        };
        match result {
            Ok(()) => {
                *last_err = None;
                if let Some(t) = trust {
                    let rows = client_rows(t);
                    if last_rows.as_ref() != Some(&rows) {
                        *last_rows = Some(rows.clone());
                        sink.send(Event::Clients(Ok(rows)));
                    }
                }
            }
            Err(e) => {
                let msg = core_err("Liste des clients illisible", &e);
                if last_err.as_ref() != Some(&msg) {
                    *last_err = Some(msg.clone());
                    sink.send(Event::Clients(Err(msg)));
                }
            }
        }
    };

    loop {
        let cmd = match rx.recv_timeout(Duration::from_millis(700)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        if let Some(cmd) = cmd {
            // Les mutations exigent une liste de confiance chargée.
            let needs_trust = matches!(cmd, DataCmd::CreateInvite { .. } | DataCmd::SetRole { .. } | DataCmd::Revoke { .. });
            if needs_trust && trust.is_none() {
                match TrustStore::load(&trust_path) {
                    Ok(t) => trust = Some(t),
                    Err(e) => {
                        sink.notice(Level::Error, core_err("Action impossible", &e));
                        continue;
                    }
                }
            }
            match cmd {
                DataCmd::CreateInvite { addr, alt_addr, ttl_ms } => {
                    let (Some(t), Some(host_key)) = (trust.as_mut(), host_pub) else {
                        sink.notice(Level::Error, "Impossible de créer l'invitation : identité du host indisponible.");
                        continue;
                    };
                    match t.create_invite(ttl_ms, now_ms()) {
                        Ok(token) => {
                            let now = now_ms();
                            let expires_ms = now.saturating_add(ttl_ms.min(grx_core::trust::MAX_INVITE_TTL_MS));
                            let invite = Invite { addr: addr.clone(), host_key, token: Some(token) };
                            let uri = invite.to_uri();
                            let uri_v6 = alt_addr.map(|a| Invite { addr: a, ..invite.clone() }.to_uri());
                            sink.send(Event::InviteCreated(InviteCreated { uri, uri_v6, addr, expires_ms }));
                        }
                        Err(e) => sink.notice(Level::Error, core_err("Création de l'invitation impossible", &e)),
                    }
                }
                DataCmd::SetRole { fingerprint, name, role } => {
                    let t = trust.as_mut().expect("chargé ci-dessus");
                    let label = match role {
                        grx_core::Role::ViewOnly => "lecture seule",
                        grx_core::Role::Full => "complet",
                        grx_core::Role::Total => "contrôle total",
                    };
                    match t.set_role(&fingerprint, role) {
                        Ok(0) => sink.notice(Level::Error, format!("« {name} » n'est plus dans la liste (déjà révoqué ?).")),
                        Ok(_) => sink.notice(
                            Level::Success,
                            format!("Rôle de « {name} » : {label}. Une session en cours est coupée, le client doit se reconnecter."),
                        ),
                        Err(e) => sink.notice(Level::Error, core_err("Changement de rôle impossible", &e)),
                    }
                    poll_trust(&mut trust, &mut last_rows, &mut last_trust_err, &sink);
                }
                DataCmd::Revoke { fingerprint, name } => {
                    let t = trust.as_mut().expect("chargé ci-dessus");
                    match t.revoke(&fingerprint) {
                        Ok(0) => sink.notice(Level::Error, format!("« {name} » n'est plus dans la liste.")),
                        Ok(_) => sink.notice(Level::Success, format!("« {name} » révoqué : sa session en cours est coupée.")),
                        Err(e) => sink.notice(Level::Error, core_err("Révocation impossible", &e)),
                    }
                    poll_trust(&mut trust, &mut last_rows, &mut last_trust_err, &sink);
                }
                DataCmd::RenameHost { old, new } => {
                    with_profiles(&mut profiles, &paths.client_dir, &sink, |p| match p.rename(&old, &new) {
                        Ok(true) => sink.notice(Level::Success, format!("« {old} » renommé en « {new} ».")),
                        Ok(false) => sink.notice(Level::Error, format!("« {old} » n'existe plus.")),
                        Err(CoreError::BadInvite(m)) => sink.notice(Level::Error, m),
                        Err(e) => sink.notice(Level::Error, core_err("Renommage impossible", &e)),
                    });
                    last_hosts_stamp = None; // force l'envoi de la liste
                }
                DataCmd::DeleteHost(name) => {
                    with_profiles(&mut profiles, &paths.client_dir, &sink, |p| match p.remove(&name) {
                        Ok(true) => sink.notice(Level::Success, format!("Host « {name} » supprimé de la liste.")),
                        Ok(false) => sink.notice(Level::Error, format!("« {name} » n'existe plus.")),
                        Err(e) => sink.notice(Level::Error, core_err("Suppression impossible", &e)),
                    });
                    last_hosts_stamp = None;
                }
                DataCmd::AutoAddr { port } => {
                    let dir = paths.host_dir.clone();
                    let sink = sink.clone();
                    // Peut prendre ~3 s (découverte de la box) : fil dédié.
                    std::thread::spawn(move || sink.send(Event::AutoAddr(auto_addr(&dir, port))));
                }
                DataCmd::DetectLocalIp { port } => match detect_local_ip() {
                    Ok(ip) => sink.send(Event::LocalIp(format!("{ip}:{port}"))),
                    Err(e) => sink.notice(Level::Error, format!("IP locale introuvable : {e}")),
                },
                DataCmd::SaveSettings(s) => {
                    if let Err(e) = settings::save(&settings_path, &s) {
                        sink.notice(Level::Error, format!("Préférences non enregistrées : {e}"));
                    }
                }
            }
        }

        poll_trust(&mut trust, &mut last_rows, &mut last_trust_err, &sink);

        // hosts.json : rechargé quand la date ou la taille change (aussi quand `grx-client` y écrit).
        let stamp = stamp_of(&hosts_path);
        if last_hosts_stamp != Some(stamp) {
            last_hosts_stamp = Some(stamp);
            match ProfileStore::load(&paths.client_dir) {
                Ok(s) => {
                    sink.send(Event::Profiles(Ok(profile_rows(&s))));
                    profiles = Some(s);
                }
                Err(e) => {
                    profiles = None;
                    sink.send(Event::Profiles(Err(core_err("Liste des hosts illisible", &e))));
                }
            }
        }
    }
}

/// Adresse publique automatique : redirection active du host (status.json), sinon IP externe
/// demandée à la box (UPnP / NAT-PMP, réseau local uniquement), avec le port du host.
pub fn auto_addr(host_dir: &Path, port: u16) -> AutoAddr {
    use grx_portmap::{plan, AddrClass, PortMapper};
    let live = hoststatus::read_live(host_dir, hoststatus::pid_alive).map(|(s, _)| s);
    let mut notes = Vec::new();
    let port = live.as_ref().and_then(|s| s.port()).unwrap_or(port);
    if let Some(s) = &live {
        if s.upnp.ok {
            if let Some(a) = &s.upnp.external_addr {
                notes.push("Adresse de la redirection active du host.".into());
                return AutoAddr { addr: Some(a.clone()), notes };
            }
        }
        if s.upnp.state == "pending" {
            notes.push("Le host cherche encore la box : réessayez dans quelques secondes.".into());
        }
    }
    match PortMapper::default().external_ip() {
        Ok(e) if e.class == AddrClass::Public => {
            notes.push(format!("IP externe lue sur la box via {}.", e.method));
            if live.is_none() {
                notes.push("Le host n'est pas lancé : démarrez-le pour que le port soit ouvert sur la box.".into());
            }
            AutoAddr { addr: Some(format!("{}:{port}", e.ip)), notes }
        }
        Ok(e) => {
            notes.push(plan::explain_non_public(e.ip, e.class));
            AutoAddr { addr: None, notes }
        }
        Err(e) => {
            notes.push(format!("IP externe introuvable (box sans UPnP/NAT-PMP ?) : {e}"));
            AutoAddr { addr: None, notes }
        }
    }
}

fn with_profiles(
    slot: &mut Option<ProfileStore>,
    dir: &Path,
    sink: &Sink,
    f: impl FnOnce(&mut ProfileStore),
) {
    if slot.is_none() {
        match ProfileStore::load(dir) {
            Ok(s) => *slot = Some(s),
            Err(e) => {
                sink.notice(Level::Error, core_err("Liste des hosts illisible", &e));
                return;
            }
        }
    }
    if let Some(s) = slot.as_mut() {
        f(s);
    }
}

// ---------------------------------------------------------------------------------------------
// Presse-papiers (en arrière-plan : un autre programme peut le garder verrouillé un instant)
// ---------------------------------------------------------------------------------------------

pub fn clipboard_copy(sink: &Sink, mut text: String, what: &'static str) {
    let sink = sink.clone();
    std::thread::spawn(move || {
        let r = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone()));
        text.zeroize();
        match r {
            Ok(()) => sink.notice(Level::Success, format!("{what} copié dans le presse-papiers.")),
            Err(e) => sink.notice(Level::Error, format!("Presse-papiers indisponible : {e}")),
        }
    });
}

pub fn clipboard_paste(sink: &Sink) {
    let sink = sink.clone();
    std::thread::spawn(move || match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(t) => sink.send(Event::Pasted(t)),
        Err(arboard::Error::ContentNotAvailable) => {
            sink.notice(Level::Error, "Le presse-papiers ne contient pas de texte.")
        }
        Err(e) => sink.notice(Level::Error, format!("Presse-papiers indisponible : {e}")),
    });
}

// ---------------------------------------------------------------------------------------------
// Processus : grx-host run
// ---------------------------------------------------------------------------------------------

pub enum HostCmd {
    Start(Vec<String>),
    Stop,
    /// Arrête le host lancé par la GUI (s'il y en a un) puis accuse réception.
    Shutdown(Sender<()>),
    /// Exécute `grx-host firewall-help` (lecture seule : affiche la commande, ne l'exécute pas).
    FirewallHelp { port: u16 },
    /// Démarre le host ÉLEVÉ via la tâche planifiée (mode Contrôle total), avec ces options de
    /// lancement écrites au préalable dans host-options.json.
    StartElevated(Vec<String>),
    /// Active le mode Contrôle total (crée la tâche, UAC) ; `at_logon` = lancer aussi à l'ouverture.
    ElevationEnable { at_logon: bool },
    /// Désactive le mode Contrôle total (supprime la tâche, UAC).
    ElevationDisable,
}

/// État du mode Contrôle total, publié à l'UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElevationState {
    pub task_present: bool,
    pub task_valid: bool,
    pub at_logon: bool,
    pub protected_location: bool,
}

pub fn spawn_host_manager(paths: Paths, sink: Sink, rx: Receiver<HostCmd>) {
    std::thread::Builder::new()
        .name("grx-host-mgr".into())
        .spawn(move || HostManager::new(paths, sink).run(rx))
        .expect("fil host");
}

struct HostManager {
    exe: PathBuf,
    host_dir: PathBuf,
    sink: Sink,
    child: Option<Child>,
    last_run_status: Option<(Option<RunStatus>, u64)>,
    last_status_read: Option<Instant>,
    log: Arc<Mutex<LogBuffer>>,
    log_dirty: Arc<AtomicBool>,
    sys: Option<sysinfo::System>,
    last_scan: Option<Instant>,
    external_pid: Option<u32>,
    stopping: bool,
    message: Option<String>,
    last_status: Option<HostStatus>,
    last_elevation: Option<ElevationState>,
    last_elevation_poll: Option<Instant>,
}

impl HostManager {
    fn new(paths: Paths, sink: Sink) -> Self {
        Self {
            exe: paths.host_exe,
            host_dir: paths.host_dir,
            last_run_status: None,
            last_status_read: None,
            sink,
            child: None,
            log: Arc::new(Mutex::new(LogBuffer::new(300))),
            log_dirty: Arc::new(AtomicBool::new(false)),
            sys: sysinfo::System::new().ok(),
            last_scan: None,
            external_pid: None,
            stopping: false,
            message: None,
            last_status: None,
            last_elevation: None,
            last_elevation_poll: None,
        }
    }

    fn run(mut self, rx: Receiver<HostCmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(HostCmd::Start(args)) => self.start(args),
                Ok(HostCmd::Stop) => self.stop(),
                Ok(HostCmd::FirewallHelp { port }) => self.firewall_help(port),
                Ok(HostCmd::StartElevated(args)) => self.start_elevated(args),
                Ok(HostCmd::ElevationEnable { at_logon }) => self.elevation_change(true, at_logon),
                Ok(HostCmd::ElevationDisable) => self.elevation_change(false, false),
                Ok(HostCmd::Shutdown(ack)) => {
                    self.stop();
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.stop();
                    return;
                }
            }
            self.tick();
        }
    }

    /// Lance `grx-host elevation <args>` (enfant non élevé ; `enable`/`disable` déclenchent l'UAC
    /// via ShellExecute depuis grx-host). Sortie capturée dans le journal. Renvoie le code.
    fn run_elevation_cmd(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--data-dir").arg(&self.host_dir).arg("elevation").args(args).stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.output()
    }

    /// Active / désactive le mode Contrôle total (une invite UAC chacun).
    fn elevation_change(&mut self, enable: bool, at_logon: bool) {
        let sub = if enable { "enable" } else { "disable" };
        let mut args = vec![sub];
        if enable && at_logon {
            args.push("--at-logon");
        }
        match self.run_elevation_cmd(&args) {
            Ok(o) if o.status.success() => {
                self.sink.notice(
                    Level::Success,
                    if enable {
                        "Contrôle total activé (host élevé par tâche planifiée)."
                    } else {
                        "Contrôle total désactivé."
                    },
                );
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                self.sink.notice(Level::Error, format!("Contrôle total : échec ({}).", if msg.is_empty() { "annulé ?" } else { &msg }));
            }
            Err(e) => self.sink.notice(Level::Error, format!("Impossible de lancer grx-host : {e}")),
        }
        self.publish_elevation();
    }

    /// Démarre le host élevé via la tâche planifiée (aucune UAC) après avoir écrit les options.
    fn start_elevated(&mut self, args: Vec<String>) {
        if self.child.is_some() || self.external_pid.is_some() {
            self.sink.notice(Level::Error, "Un host est déjà en cours d'exécution.");
            return;
        }
        if let Err(e) = write_host_options(&self.host_dir, &args) {
            self.sink.notice(Level::Error, format!("Options du host élevé non écrites : {e}"));
            return;
        }
        match self.run_elevation_cmd(&["run-task"]) {
            Ok(o) if o.status.success() => {
                self.message = Some("Host élevé démarré (mode Contrôle total).".into());
                self.last_scan = None;
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                self.sink.notice(Level::Error, format!("Démarrage élevé impossible : {}", if msg.is_empty() { "la tâche existe-t-elle ?" } else { &msg }));
            }
            Err(e) => self.sink.notice(Level::Error, format!("Impossible de lancer grx-host : {e}")),
        }
    }

    /// Interroge `grx-host elevation status` et publie l'état.
    fn publish_elevation(&mut self) {
        let st = match self.run_elevation_cmd(&["status"]) {
            Ok(o) => parse_elevation_status(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => ElevationState::default(),
        };
        if self.last_elevation.as_ref() != Some(&st) {
            self.last_elevation = Some(st.clone());
            self.sink.send(Event::Elevation(st));
        }
    }

    fn push_log(&self, line: &str) {
        if let Ok(mut l) = self.log.lock() {
            l.push(line);
        }
        self.log_dirty.store(true, Ordering::Relaxed);
    }

    fn start(&mut self, args: Vec<String>) {
        if self.child.is_some() || self.external_pid.is_some() {
            self.sink.notice(Level::Error, "Un host est déjà en cours d'exécution.");
            return;
        }
        if !self.exe.exists() && self.exe.components().count() > 1 {
            self.sink.notice(
                Level::Error,
                format!("grx-host.exe introuvable ({}). Il doit se trouver à côté de grx-gui.exe.", self.exe.display()),
            );
            return;
        }
        if let Ok(mut l) = self.log.lock() {
            l.clear();
        }
        self.message = None;
        let mut cmd = Command::new(&self.exe);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Console cachée : nécessaire pour pouvoir lui envoyer Ctrl+C à l'arrêt.
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(o) = child.stdout.take() {
                    spawn_line_reader(o, self.log.clone(), self.log_dirty.clone(), self.sink.clone());
                }
                if let Some(e) = child.stderr.take() {
                    spawn_line_reader(e, self.log.clone(), self.log_dirty.clone(), self.sink.clone());
                }
                self.child = Some(child);
                self.last_scan = None;
            }
            Err(e) => {
                self.sink.notice(Level::Error, format!("Impossible de lancer grx-host.exe : {e}"));
            }
        }
    }

    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            // Host élevé lancé par la tâche planifiée : pas un enfant de la GUI, et non élevé on
            // ne peut pas lui envoyer Ctrl+C. On déclenche son arrêt propre par événement nommé
            // (grx-host elevation stop --pid), valable seulement si le mode Contrôle total est actif.
            if let Some(pid) = self.external_pid {
                if self.last_elevation.as_ref().is_some_and(|e| e.task_present) {
                    self.stopping = true;
                    self.publish(Some(pid));
                    self.push_log("— Arrêt demandé au host élevé (événement nommé) —");
                    let ok = matches!(self.run_elevation_cmd(&["stop", "--pid", &pid.to_string()]), Ok(o) if o.status.success());
                    if !ok {
                        self.sink.notice(Level::Error, "Arrêt du host élevé impossible (déjà arrêté ?).");
                    }
                    self.stopping = false;
                    self.last_scan = None;
                    self.publish(None);
                } else {
                    self.sink.notice(
                        Level::Error,
                        "Ce host n'a pas été lancé par cette fenêtre : fermez-le depuis son terminal (Ctrl+C).",
                    );
                }
            }
            return;
        };
        self.stopping = true;
        // Pousse l'état « arrêt en cours » avant l'attente.
        let pid = child.id();
        self.publish(Some(pid));
        self.push_log("— Arrêt demandé (Ctrl+C) —");
        let clean = send_ctrl_c(pid) && wait_exit(&mut child, Duration::from_secs(6));
        if clean {
            self.message = Some("Host arrêté proprement.".into());
        } else {
            let _ = child.kill();
            let _ = child.wait();
            self.message = Some("Le host ne répondait pas à l'arrêt : il a été terminé de force.".into());
            self.push_log("— Arrêt forcé —");
        }
        self.stopping = false;
        self.last_scan = None;
        self.publish(None);
    }

    fn tick(&mut self) {
        // Fin spontanée du processus lancé par la GUI (erreur de port, plantage...).
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let tail = self.log.lock().map(|l| l.tail(3).join(" | ")).unwrap_or_default();
                    self.message = Some(match status.code() {
                        Some(0) => "Le host s'est arrêté.".to_string(),
                        Some(c) => format!("Le host s'est arrêté (code {c}). {tail}"),
                        None => "Le host s'est arrêté brutalement.".to_string(),
                    });
                    self.child = None;
                    self.last_scan = None;
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        // Détection d'un host lancé hors de la GUI (terminal, autre session...).
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.last_scan = Some(Instant::now());
            self.external_pid = self.scan_external();
        }
        self.publish(None);
        self.poll_status_file();
        // État du mode Contrôle total : relu toutes les ~3 s (schtasks est peu coûteux mais pas gratuit).
        if self.last_elevation_poll.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.last_elevation_poll = Some(Instant::now());
            self.publish_elevation();
        }
        if self.log_dirty.swap(false, Ordering::Relaxed) {
            let snap = self.log.lock().map(|l| l.snapshot()).unwrap_or_default();
            self.sink.send(Event::HostLog(snap));
        }
    }

    /// `status.json` relu environ chaque seconde ; envoyé quand il change (ou toutes les 30 s pour le bail).
    fn poll_status_file(&mut self) {
        if self.last_status_read.is_some_and(|t| t.elapsed() < Duration::from_millis(900)) {
            return;
        }
        self.last_status_read = Some(Instant::now());
        let live = hoststatus::read_live(&self.host_dir, hoststatus::pid_alive);
        let key = (live.as_ref().map(|(s, _)| s.clone()), live.as_ref().map(|(_, a)| a.as_secs() / 30).unwrap_or(0));
        if self.last_run_status.as_ref() != Some(&key) {
            self.last_run_status = Some(key);
            self.sink.send(Event::RunStatus(live));
        }
    }

    fn firewall_help(&self, port: u16) {
        let exe = self.exe.clone();
        let dir = self.host_dir.clone();
        let sink = self.sink.clone();
        std::thread::spawn(move || {
            let mut cmd = Command::new(&exe);
            cmd.arg("--data-dir").arg(&dir).args(["firewall-help", "--port", &port.to_string()]);
            cmd.stdin(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let r = match cmd.output() {
                Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                Ok(o) => Err(format!("grx-host firewall-help a échoué : {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => Err(format!("Impossible de lancer grx-host.exe : {e}")),
            };
            sink.send(Event::FirewallHelp(r));
        });
    }

    fn scan_external(&mut self) -> Option<u32> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
        let sys = self.sys.as_mut()?;
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet),
        );
        let own = self.child.as_ref().map(|c| c.id());
        sys.processes()
            .iter()
            .filter(|(pid, p)| {
                Some(pid.as_u32()) != own
                    && p.name().to_string_lossy().eq_ignore_ascii_case(&format!("grx-host{}", std::env::consts::EXE_SUFFIX))
                    && p.cmd().iter().any(|a| a == "run")
            })
            .map(|(pid, _)| pid.as_u32())
            .min()
    }

    fn publish(&mut self, forced_pid: Option<u32>) {
        let own = self.child.as_ref().map(|c| c.id()).or(forced_pid.filter(|_| self.stopping));
        let status = HostStatus {
            running: own.is_some() || self.external_pid.is_some(),
            pid: own.or(self.external_pid),
            ours: own.is_some(),
            stopping: self.stopping,
            message: self.message.clone(),
        };
        if self.last_status.as_ref() != Some(&status) {
            self.last_status = Some(status.clone());
            self.sink.send(Event::Host(status));
        }
    }
}

fn spawn_line_reader<R: std::io::Read + Send + 'static>(
    r: R,
    log: Arc<Mutex<LogBuffer>>,
    dirty: Arc<AtomicBool>,
    sink: Sink,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            if let Ok(mut l) = log.lock() {
                l.push(&line);
            }
            dirty.store(true, Ordering::Relaxed);
            sink.ctx.request_repaint();
        }
    });
}

fn wait_exit(child: &mut Child, timeout: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    false
}

/// Envoie Ctrl+C à la console (cachée) du processus enfant. Le processus courant (sous-système
/// Windows) n'a pas de console : on s'attache brièvement à celle de l'enfant en ignorant Ctrl+C
/// pour nous-mêmes, on émet l'événement, puis on se détache.
#[cfg(windows)]
fn send_ctrl_c(pid: u32) -> bool {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
    };
    // SAFETY : appels Win32 sans pointeur ; l'état de console du processus n'est modifié que par ce fil.
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return false;
        }
        SetConsoleCtrlHandler(None, 1);
        let ok = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
        std::thread::sleep(Duration::from_millis(150));
        FreeConsole();
        SetConsoleCtrlHandler(None, 0);
        ok
    }
}

#[cfg(not(windows))]
fn send_ctrl_c(_pid: u32) -> bool {
    false
}

// ---------------------------------------------------------------------------------------------
// Processus : grx-client play
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct ClientSessions {
    next_id: Arc<AtomicU64>,
    pub kill_all: Arc<AtomicBool>,
    pub active: Arc<AtomicUsize>,
}

impl ClientSessions {
    /// Lance `grx-client` avec `args`. `stdin_line` (lien ou nom) est écrit sur son entrée standard
    /// puis effacé de la mémoire : le jeton n'apparaît pas dans la ligne de commande.
    pub fn launch(&self, exe: PathBuf, args: Vec<String>, mut stdin_line: String, name: String, sink: Sink) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kill = self.kill_all.clone();
        let active = self.active.clone();
        active.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let exit = |error: Option<String>, code: Option<i32>, tail: Vec<String>| {
                active.fetch_sub(1, Ordering::SeqCst);
                sink.send(Event::Session { id, name: name.clone(), state: SessionState::Exited { code, tail, error } });
            };
            let mut cmd = Command::new(&exe);
            cmd.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    stdin_line.zeroize();
                    return exit(
                        Some(format!("Impossible de lancer grx-client.exe ({e}). Il doit se trouver à côté de grx-gui.exe.")),
                        None,
                        vec![],
                    );
                }
            };
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(stdin_line.as_bytes());
                let _ = si.write_all(b"\n");
            }
            stdin_line.zeroize();
            sink.send(Event::Session { id, name: name.clone(), state: SessionState::Started });

            let log = Arc::new(Mutex::new(LogBuffer::new(50)));
            let dirty = Arc::new(AtomicBool::new(false));
            let mut readers = Vec::new();
            if let Some(o) = child.stdout.take() {
                readers.push(spawn_reader_joinable(o, log.clone(), dirty.clone()));
            }
            if let Some(e) = child.stderr.take() {
                readers.push(spawn_reader_joinable(e, log.clone(), dirty.clone()));
            }
            let status = loop {
                if kill.load(Ordering::SeqCst) {
                    let _ = child.kill();
                }
                match child.try_wait() {
                    Ok(Some(s)) => break Ok(s),
                    Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                    Err(e) => break Err(e),
                }
            };
            for r in readers {
                let _ = r.join();
            }
            let tail = log.lock().map(|l| l.tail(6)).unwrap_or_default();
            match status {
                Ok(s) => exit(None, s.code(), tail),
                Err(e) => exit(Some(format!("Suivi du processus impossible : {e}")), None, tail),
            }
        });
    }
}

fn spawn_reader_joinable<R: std::io::Read + Send + 'static>(
    r: R,
    log: Arc<Mutex<LogBuffer>>,
    dirty: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            if let Ok(mut l) = log.lock() {
                l.push(&line);
            }
            dirty.store(true, Ordering::Relaxed);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for<T>(rx: &Receiver<Event>, mut f: impl FnMut(Event) -> Option<T>) -> T {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                if let Some(v) = f(ev) {
                    return v;
                }
            }
        }
        panic!("événement attendu non reçu");
    }

    /// Chemin réel de la GUI : invitation -> consommation par un « client » (autre processus simulé
    /// par un second TrustStore) -> apparition automatique -> rôle -> révocation, le tout via TrustStore.
    #[test]
    fn data_worker_invite_role_revoke_and_auto_refresh() {
        let base = std::env::temp_dir().join(format!("grx-gui-backend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths {
            host_dir: base.join("host"),
            client_dir: base.join("client"),
            host_exe: PathBuf::from("absent"),
            client_exe: PathBuf::from("absent"),
        };
        let (sink, rx) = Sink::new(egui::Context::default());
        let (tx, wrx) = channel();
        spawn_data_worker(paths.clone(), sink, wrx);
        let host_fp = wait_for(&rx, |e| match e {
            Event::HostIdentity(Ok(fp)) => Some(fp),
            _ => None,
        });
        wait_for(&rx, |e| matches!(e, Event::Clients(Ok(ref r)) if r.is_empty()).then_some(()));

        tx.send(DataCmd::CreateInvite {
            addr: "127.0.0.1:7777".into(),
            alt_addr: Some("[2001:db8::1]:7777".into()),
            ttl_ms: 300_000,
        })
        .unwrap();
        let (uri, uri_v6, expires) = wait_for(&rx, |e| match e {
            Event::InviteCreated(ic) => Some((ic.uri.clone(), ic.uri_v6.clone(), ic.expires_ms)),
            _ => None,
        });
        let v6 = Invite::parse(&uri_v6.unwrap()).unwrap();
        assert_eq!(v6.addr, "[2001:db8::1]:7777");
        assert!(expires > now_ms());
        let inv = Invite::parse(&uri).unwrap();
        assert_eq!(fingerprint(&inv.host_key), host_fp);
        assert_eq!(v6.token, inv.token, "même jeton pour les deux liens");

        // Un autre processus (le host) consomme l'invitation et autorise le client.
        let mut other = TrustStore::load(&paths.host_dir.join("trust.json")).unwrap();
        assert!(other.consume_invite(inv.token.as_ref().unwrap(), now_ms()).unwrap());
        let client = Identity::generate();
        other.authorize(&client.public(), "pc-test", now_ms()).unwrap();
        let rows = wait_for(&rx, |e| match e {
            Event::Clients(Ok(r)) if !r.is_empty() => Some(r),
            _ => None,
        });
        assert_eq!(rows[0].name, "pc-test");
        assert_eq!(rows[0].fingerprint, client.fingerprint());
        assert_eq!(rows[0].role, grx_core::Role::Full);
        // Pas de secret sur disque : le lien en clair n'est nulle part dans trust.json.
        let disk = std::fs::read_to_string(paths.host_dir.join("trust.json")).unwrap();
        assert!(!disk.contains(&inv.token.as_ref().unwrap().to_b32()));

        tx.send(DataCmd::SetRole { fingerprint: client.fingerprint(), name: "pc-test".into(), role: grx_core::Role::Total })
            .unwrap();
        wait_for(&rx, |e| matches!(e, Event::Clients(Ok(ref r)) if r.len() == 1 && r[0].role == grx_core::Role::Total).then_some(()));
        assert_eq!(
            TrustStore::load(&paths.host_dir.join("trust.json")).unwrap().role_of(&client.public()),
            Some(grx_core::Role::Total)
        );

        tx.send(DataCmd::Revoke { fingerprint: client.fingerprint(), name: "pc-test".into() }).unwrap();
        wait_for(&rx, |e| matches!(e, Event::Clients(Ok(ref r)) if r.is_empty()).then_some(()));
        assert!(!TrustStore::load(&paths.host_dir.join("trust.json")).unwrap().is_authorized(&client.public()));

        // hosts.json : écrit par grx-client (simulé), renommé et supprimé par la GUI.
        let mut ps = ProfileStore::load(&paths.client_dir).unwrap();
        ps.remember(&Invite { addr: "h:1".into(), host_key: inv.host_key, token: None }, Some("salon".into())).unwrap();
        wait_for(&rx, |e| matches!(e, Event::Profiles(Ok(ref r)) if r.len() == 1).then_some(()));
        tx.send(DataCmd::RenameHost { old: "salon".into(), new: "bureau".into() }).unwrap();
        wait_for(&rx, |e| matches!(e, Event::Profiles(Ok(ref r)) if r.len() == 1 && r[0].name == "bureau").then_some(()));
        tx.send(DataCmd::DeleteHost("bureau".into())).unwrap();
        wait_for(&rx, |e| matches!(e, Event::Profiles(Ok(ref r)) if r.is_empty()).then_some(()));
        drop(tx);
        std::thread::sleep(Duration::from_millis(900));
        let _ = std::fs::remove_dir_all(&base);
    }
}
