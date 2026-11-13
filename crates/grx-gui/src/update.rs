//! Mises à jour dans la GUI : vérification (au démarrage au plus une fois par jour, ou à la
//! demande), bandeau « Version X disponible », téléchargement vérifié et lancement du setup.
//!
//! Toute la logique de sécurité est dans `grx-update` (signature Ed25519, SHA-256, anti-downgrade).
//! Ici : fils d'arrière-plan + état d'affichage. Aucune requête réseau si la vérification est
//! désactivée (réglage) et que l'utilisateur ne clique pas sur « Rechercher des mises à jour ».

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, RichText};
use grx_update::{guard, install, Available, CheckOutcome, InstallMode, UpdateConfig, UpdateError, UreqFetcher};
use serde::{Deserialize, Serialize};

use crate::backend::{Event, Sink};
use crate::theme::{card, colored, dot, muted, primary_button, small_muted, ERR, MUTED, OK, WARN};

/// Intervalle minimal entre deux vérifications automatiques.
pub const AUTO_CHECK_INTERVAL_MS: u64 = 24 * 3600 * 1000;

/// Préférences (dans `gui.json`, sans secret).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct UpdatePrefs {
    /// Vérifier au démarrage (au plus une fois par jour).
    pub auto_check: bool,
    /// Dernière vérification automatique ou manuelle (ms depuis l'epoch).
    pub last_check_ms: u64,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self { auto_check: true, last_check_ms: 0 }
    }
}

/// Faut-il lancer la vérification automatique maintenant ?
pub fn auto_check_due(prefs: &UpdatePrefs, now_ms: u64) -> bool {
    prefs.auto_check
        && (prefs.last_check_ms == 0 || prefs.last_check_ms > now_ms || now_ms - prefs.last_check_ms >= AUTO_CHECK_INTERVAL_MS)
}

pub enum UpdateEvent {
    Checked { manual: bool, result: Result<CheckOutcome, UpdateError> },
    /// Des sessions empêchent l'installation (on attend leur fin).
    Waiting(Vec<String>),
    Progress(u64, u64),
    /// Setup lancé : la GUI doit se fermer pour le laisser remplacer les fichiers.
    Launched,
    Failed(String),
}

#[derive(Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate(String),
    Available(Box<Available>),
    Manual { latest: String, min: String },
    Waiting { avail: Box<Available>, reasons: Vec<String> },
    Downloading { version: String, done: u64, total: u64 },
    Launched,
    Error(String),
    NotConfigured,
}

pub struct UpdateUi {
    pub state: UpdateState,
    /// Bandeau masqué par « Plus tard » (pour cette exécution).
    pub dismissed: bool,
    pub mode: InstallMode,
    pub install_dir: Option<PathBuf>,
    cancel_wait: Arc<AtomicBool>,
}

impl Default for UpdateUi {
    fn default() -> Self {
        Self {
            state: UpdateState::Idle,
            dismissed: false,
            mode: install::detect_current(),
            install_dir: install::current_exe_dir(),
            cancel_wait: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub fn current_version() -> &'static str {
    grx_update::config::CURRENT_VERSION
}

/// Configuration réelle (dépôt, clé intégrée, `GRX_UPDATE_URL`).
fn config() -> Result<UpdateConfig, UpdateError> {
    UpdateConfig::from_env()
}

/// Les mises à jour sont-elles configurées ? (sinon : aucune requête n'est jamais faite)
pub fn configured() -> bool {
    config().map(|c| c.is_configured()).unwrap_or(false)
}

/// Vérification en arrière-plan.
pub fn spawn_check(sink: &Sink, manual: bool) {
    let sink = sink.clone();
    std::thread::Builder::new()
        .name("grx-update-check".into())
        .spawn(move || {
            let result = config().and_then(|cfg| {
                let fetcher = UreqFetcher::new(cfg.allow_loopback_http);
                grx_update::check(&cfg, &fetcher)
            });
            sink.send(Event::Update(UpdateEvent::Checked { manual, result }));
        })
        .ok();
}

/// Installation : attend la fin des sessions (sauf `force_stop`), télécharge, vérifie, lance.
pub fn spawn_install(sink: &Sink, avail: Available, ui: &UpdateUi, force_stop: bool) {
    let sink = sink.clone();
    let mode = ui.mode;
    let dir = ui.install_dir.clone();
    ui.cancel_wait.store(false, Ordering::SeqCst);
    let cancel = ui.cancel_wait.clone();
    std::thread::Builder::new()
        .name("grx-update-install".into())
        .spawn(move || {
            if let Some(dir) = &dir {
                if force_stop {
                    guard::stop_in_dir(dir, Duration::from_secs(6));
                }
                let mut announced = Vec::new();
                loop {
                    let reasons = guard::busy(dir);
                    if reasons.is_empty() {
                        break;
                    }
                    if cancel.load(Ordering::SeqCst) {
                        return;
                    }
                    if reasons != announced {
                        announced = reasons.clone();
                        sink.send(Event::Update(UpdateEvent::Waiting(reasons)));
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
            let res = (|| -> Result<(), String> {
                let cfg = config().map_err(|e| e.to_string())?;
                let fetcher = UreqFetcher::new(cfg.allow_loopback_http);
                let tmp = grx_update::fresh_download_dir().map_err(|e| e.to_string())?;
                let mut last = 0u64;
                let setup = grx_update::download(&avail, &fetcher, &tmp, &mut |done, total| {
                    if done - last >= 256 * 1024 || done == total {
                        last = done;
                        sink.send(Event::Update(UpdateEvent::Progress(done, total)));
                    }
                })
                .map_err(|e| e.to_string())?;
                install::launch(&setup, mode, true).map_err(|e| e.to_string())?;
                Ok(())
            })();
            sink.send(Event::Update(match res {
                Ok(()) => UpdateEvent::Launched,
                Err(e) => UpdateEvent::Failed(e),
            }));
        })
        .ok();
}

impl UpdateUi {
    pub fn on_event(&mut self, ev: UpdateEvent) {
        match ev {
            UpdateEvent::Checked { manual, result } => {
                self.state = match result {
                    Ok(CheckOutcome::UpToDate { latest }) => UpdateState::UpToDate(latest.to_string()),
                    Ok(CheckOutcome::Available(a)) => {
                        self.dismissed = false;
                        UpdateState::Available(Box::new(a))
                    }
                    Ok(CheckOutcome::ManualRequired { latest, min_version }) => {
                        self.dismissed = false;
                        UpdateState::Manual { latest: latest.to_string(), min: min_version.to_string() }
                    }
                    Err(UpdateError::NotConfigured) => UpdateState::NotConfigured,
                    // Vérification automatique : une erreur réseau n'est pas affichée en bandeau.
                    Err(e) => {
                        self.dismissed = !manual;
                        UpdateState::Error(format!("Recherche de mise à jour : {e}"))
                    }
                };
            }
            UpdateEvent::Waiting(reasons) => {
                let prev = std::mem::take(&mut self.state);
                self.state = match prev {
                    UpdateState::Available(avail) | UpdateState::Waiting { avail, .. } => UpdateState::Waiting { avail, reasons },
                    other => other,
                };
            }
            UpdateEvent::Progress(done, total) => {
                let version = match &self.state {
                    UpdateState::Waiting { avail, .. } | UpdateState::Available(avail) => avail.manifest.version.to_string(),
                    UpdateState::Downloading { version, .. } => version.clone(),
                    _ => String::new(),
                };
                self.state = UpdateState::Downloading { version, done, total };
            }
            UpdateEvent::Launched => self.state = UpdateState::Launched,
            UpdateEvent::Failed(e) => {
                self.dismissed = false;
                self.state = UpdateState::Error(format!("Mise à jour annulée : {e}"));
            }
        }
    }

    pub fn checking(&self) -> bool {
        matches!(self.state, UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Waiting { .. })
    }

    /// Bandeau (affiché en haut de la fenêtre). Renvoie l'action demandée.
    pub fn banner(&mut self, ui: &mut egui::Ui) -> BannerAction {
        let mut action = BannerAction::None;
        if self.dismissed {
            return action;
        }
        let show = matches!(
            self.state,
            UpdateState::Available(_)
                | UpdateState::Manual { .. }
                | UpdateState::Waiting { .. }
                | UpdateState::Downloading { .. }
                | UpdateState::Launched
                | UpdateState::Error(_)
        );
        if !show {
            return action;
        }
        card(ui, "Mise à jour", |ui| match &self.state {
            UpdateState::Available(a) => {
                ui.horizontal_wrapped(|ui| {
                    dot(ui, OK);
                    ui.label(
                        RichText::new(format!("Version {} disponible", a.manifest.version)).strong().size(16.0),
                    );
                    ui.label(muted(format!("(installée : {}, publiée le {})", current_version(), short_date(&a.manifest.published))));
                });
                if !a.manifest.notes.is_empty() {
                    egui::ScrollArea::vertical().max_height(120.0).id_salt("update-notes").show(ui, |ui| {
                        ui.label(&a.manifest.notes);
                    });
                }
                ui.label(small_muted(format!(
                    "Signature vérifiée. {} Mo. {}",
                    (a.manifest.size + 512 * 1024) / (1024 * 1024),
                    match self.mode {
                        InstallMode::AllUsers => "Installation pour tous les utilisateurs : Windows demandera l'autorisation administrateur.",
                        InstallMode::CurrentUser => "La fenêtre se fermera pendant l'installation puis se rouvrira.",
                        InstallMode::NotInstalled => "Copie non installée : l'installateur s'ouvrira normalement.",
                    }
                )));
                ui.horizontal(|ui| {
                    if ui.add(primary_button("Installer maintenant")).clicked() {
                        action = BannerAction::Install { force_stop: false };
                    }
                    if ui.button("Plus tard").clicked() {
                        action = BannerAction::Dismiss;
                    }
                });
            }
            UpdateState::Manual { latest, min } => {
                ui.horizontal_wrapped(|ui| {
                    dot(ui, WARN);
                    ui.label(colored(
                        format!("Version {latest} disponible, mais elle ne peut pas être installée automatiquement depuis la {} (minimum {min}) : téléchargez l'installateur sur la page des releases.", current_version()),
                        WARN,
                    ));
                });
                if ui.button("Plus tard").clicked() {
                    action = BannerAction::Dismiss;
                }
            }
            UpdateState::Waiting { reasons, .. } => {
                ui.horizontal_wrapped(|ui| {
                    dot(ui, WARN);
                    ui.label(colored("Installation après la session : elle démarrera toute seule dès que plus rien ne tourne.", WARN));
                });
                for r in reasons {
                    ui.label(muted(format!("• {r}")));
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("Arrêter le host / les sessions et installer maintenant")
                        .on_hover_text("Un joueur connecté sera déconnecté.")
                        .clicked()
                    {
                        action = BannerAction::Install { force_stop: true };
                    }
                    if ui.button("Annuler").clicked() {
                        action = BannerAction::CancelWait;
                    }
                });
            }
            UpdateState::Downloading { version, done, total } => {
                ui.label(format!("Téléchargement de la version {version}…"));
                let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
            }
            UpdateState::Launched => {
                ui.label(colored("Installateur lancé : la fenêtre va se fermer.", OK));
            }
            UpdateState::Error(e) => {
                ui.horizontal_wrapped(|ui| {
                    dot(ui, ERR);
                    ui.label(colored(e, ERR));
                    if ui.small_button("Fermer").clicked() {
                        action = BannerAction::Dismiss;
                    }
                });
            }
            _ => {}
        });
        action
    }

    /// Contenu du menu « Mises à jour » de l'en-tête.
    pub fn menu(&mut self, ui: &mut egui::Ui, prefs: &mut UpdatePrefs) -> MenuAction {
        let mut action = MenuAction::None;
        ui.label(RichText::new(format!("GamingRemoteExperience {}", current_version())).strong());
        ui.label(small_muted(self.mode.label()));
        ui.separator();
        let can_check = !self.checking() && !matches!(self.state, UpdateState::Launched);
        if ui.add_enabled(can_check, egui::Button::new("Rechercher des mises à jour")).clicked() {
            action = MenuAction::CheckNow;
        }
        match &self.state {
            UpdateState::Checking => {
                ui.label(muted("Recherche en cours…"));
            }
            UpdateState::UpToDate(v) => {
                ui.label(colored(format!("À jour (dernière version publiée : {v})"), OK));
            }
            UpdateState::NotConfigured => {
                ui.label(colored("Mises à jour pas encore configurées pour cette version.", MUTED));
            }
            UpdateState::Error(e) => {
                ui.label(colored(e, ERR));
            }
            _ => {}
        }
        if ui.checkbox(&mut prefs.auto_check, "Rechercher au démarrage (une fois par jour)").changed() {
            action = MenuAction::PrefsChanged;
        }
        ui.label(small_muted("Désactivé : aucune requête réseau n'est faite sans votre clic."));
        action
    }

    pub fn cancel_wait(&mut self) {
        self.cancel_wait.store(true, Ordering::SeqCst);
        let prev = std::mem::take(&mut self.state);
        self.state = match prev {
            UpdateState::Waiting { avail, .. } => UpdateState::Available(avail),
            other => other,
        };
    }
}

pub enum BannerAction {
    None,
    Install { force_stop: bool },
    Dismiss,
    CancelWait,
}

pub enum MenuAction {
    None,
    CheckNow,
    PrefsChanged,
}

fn short_date(rfc3339: &str) -> String {
    rfc3339.get(..10).unwrap_or(rfc3339).to_string()
}

// ---------------------------------------------------------------------------------------------
// Modes sans fenêtre (installateur et tests)
// ---------------------------------------------------------------------------------------------

/// `grx-gui --stop-grx-processes` : appelé par l'installateur avant mise à jour / désinstallation.
/// Arrête proprement les grx-host / grx-client de CE dossier (Ctrl+C), puis ferme les autres GUI.
pub fn stop_processes_cli() -> i32 {
    match install::current_exe_dir() {
        Some(dir) => {
            let left = guard::stop_in_dir(&dir, Duration::from_secs(6));
            if left == 0 { 0 } else { 1 }
        }
        None => 1,
    }
}

/// `grx-gui --update-now` : vérification + installation sans fenêtre (tests de bout en bout).
/// Codes de sortie : 0 setup lancé, 2 déjà à jour, 3 sessions en cours, 4 installation manuelle
/// requise, 1 erreur. Le setup n'est pas relancé en fin d'installation (pas de `/RELAUNCH`).
pub fn update_now_cli() -> i32 {
    let res = (|| -> Result<i32, String> {
        let cfg = config().map_err(|e| e.to_string())?;
        let fetcher = UreqFetcher::new(cfg.allow_loopback_http);
        match grx_update::check(&cfg, &fetcher).map_err(|e| e.to_string())? {
            CheckOutcome::UpToDate { latest } => {
                println!("à jour (installée {}, publiée {latest})", current_version());
                Ok(2)
            }
            CheckOutcome::ManualRequired { latest, min_version } => {
                println!("version {latest} : installation manuelle requise (minimum {min_version})");
                Ok(4)
            }
            CheckOutcome::Available(a) => {
                let dir = install::current_exe_dir().ok_or("dossier de l'exécutable introuvable")?;
                let busy = guard::busy(&dir);
                if !busy.is_empty() {
                    println!("installation reportée : {}", busy.join(" ; "));
                    return Ok(3);
                }
                let tmp = grx_update::fresh_download_dir().map_err(|e| e.to_string())?;
                let setup = grx_update::download(&a, &fetcher, &tmp, &mut |_, _| {}).map_err(|e| e.to_string())?;
                let mode = install::detect_current();
                println!("version {} vérifiée, lancement du setup ({})", a.manifest.version, mode.label());
                install::launch(&setup, mode, false).map_err(|e| e.to_string())?;
                Ok(0)
            }
        }
    })();
    match res {
        Ok(code) => code,
        Err(e) => {
            println!("erreur : {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_check_at_most_daily_and_disableable() {
        let day = AUTO_CHECK_INTERVAL_MS;
        let p = UpdatePrefs { auto_check: true, last_check_ms: 10 * day };
        assert!(!auto_check_due(&p, 10 * day + day - 1));
        assert!(auto_check_due(&p, 11 * day));
        assert!(auto_check_due(&UpdatePrefs::default(), 5));
        // Horloge revenue en arrière : on revérifie plutôt que d'attendre indéfiniment.
        assert!(auto_check_due(&p, day));
        let off = UpdatePrefs { auto_check: false, last_check_ms: 0 };
        assert!(!auto_check_due(&off, 100 * day));
    }

    #[test]
    fn prefs_default_to_enabled_when_missing_from_json() {
        let p: UpdatePrefs = serde_json::from_str("{}").unwrap();
        assert!(p.auto_check);
    }
}
