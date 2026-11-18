//! Interface des demandes d'accès sans invitation (host et client) et du test de joignabilité.
//! La logique (analyse, sonde réseau) est dans `access.rs` ; les E/S passent par `backend`.

use std::time::Duration;

use eframe::egui::{self, RichText, TextEdit};
use grx_core::trust::REQUESTS_ALWAYS_OPEN;
use grx_core::util::now_ms;
use grx_core::Role;

use super::App;
use crate::access::{AccessReqUi, ReachResult, ReqPhase, RequestRow};
use crate::backend::{DataCmd, Level, SessionState};
use crate::logic::{self, format_countdown, format_date_local, validate_addr, validate_name, DEFAULT_PORT};
use crate::theme::{card, colored, danger_button, dot, muted, primary_button, small_muted, ERR, MUTED, OK, WARN};

/// Validation d'une demande : rôle choisi dans la fenêtre de confirmation.
#[derive(Clone)]
pub struct ApproveDialog {
    pub row: RequestRow,
    pub role: Role,
}

/// État de l'interface des demandes d'accès (champs de `App`).
#[derive(Default)]
pub struct AccessUi {
    pub requests: crate::access::RequestsState,
    pub requests_loaded: bool,
    pub approve: Option<ApproveDialog>,
    pub reach: Option<Vec<crate::access::ReachLine>>,
    pub reach_pending: bool,
    /// Demande d'accès lancée par ce client.
    pub req: Option<AccessReqUi>,
    pub req_addr: String,
    pub req_name: String,
    pub req_name_touched: bool,
}

impl App {
    // ───────────────────────────── événements ─────────────────────────────

    pub(super) fn on_requests(&mut self, st: crate::access::RequestsState) {
        if self.access.requests_loaded {
            let known: Vec<&str> = self.access.requests.rows.iter().map(|r| r.fingerprint.as_str()).collect();
            let fresh: Vec<RequestRow> = st.pending().filter(|r| !known.contains(&r.fingerprint.as_str())).cloned().collect();
            for r in fresh {
                self.notify(Level::Info, format!("Demande d'accès de « {} » ({}) : à valider dans l'onglet Host.", r.name, r.addr));
            }
        }
        self.access.requests_loaded = true;
        self.access.requests = st;
    }

    /// Fin ou démarrage du processus `grx-client request`. `true` si l'événement le concernait.
    pub(super) fn on_access_req_session(&mut self, id: u64, state: &SessionState) -> bool {
        let Some(req) = self.access.req.clone() else { return false };
        if req.id != id {
            return false;
        }
        let SessionState::Exited { code, tail, error } = state else { return true };
        let detail = logic::summarize_tail(tail, 2);
        if req.cancelled {
            self.access.req = None;
            self.notify(Level::Info, "Demande d'accès annulée.");
            return true;
        }
        match (error, code, req.phase) {
            (Some(e), _, _) => {
                self.access.req = None;
                self.notify(Level::Error, e.clone());
            }
            (None, Some(0), ReqPhase::Accepted) => {
                let name = req.saved_as.clone().unwrap_or(req.name.clone());
                self.access.req = None;
                self.access.req_addr.clear();
                self.access.req_name.clear();
                self.access.req_name_touched = false;
                self.notify(Level::Success, format!("Accès accordé : host mémorisé sous « {name} ». Connexion…"));
                let args = self.settings.client.to_args(&self.paths.client_dir, "-", None);
                self.launch_client(name.clone(), args, name);
            }
            (None, Some(4), _) => {
                if let Some(r) = self.access.req.as_mut() {
                    r.phase = ReqPhase::Denied;
                }
                self.notify(Level::Error, "Demande refusée par le propriétaire du host.");
            }
            (None, _, _) => {
                self.access.req = None;
                self.notify(Level::Error, format!("Demande d'accès interrompue. {detail}"));
            }
        }
        true
    }

    // ───────────────────────────── host ─────────────────────────────

    pub(super) fn requests_card(&mut self, ui: &mut egui::Ui) {
        let now = now_ms();
        let st = self.access.requests.clone();
        let running = self.host.running;
        card(ui, "Demandes d'accès (sans lien d'invitation)", |ui| {
            ui.horizontal_wrapped(|ui| {
                if st.is_open(now) {
                    dot(ui, WARN);
                    if st.open_until_ms == REQUESTS_ALWAYS_OPEN {
                        ui.label(colored("Ouvertes sans limite de durée : le host répond aux inconnus", WARN));
                    } else {
                        let left = logic::remaining_secs(st.open_until_ms, now);
                        ui.label(colored(format!("Ouvertes encore {}", format_countdown(left)), WARN));
                        ui.ctx().request_repaint_after(Duration::from_secs(1));
                    }
                } else {
                    dot(ui, MUTED);
                    ui.label(muted("Fermées : le host reste muet envers les inconnus (recommandé)"));
                }
            });
            ui.horizontal_wrapped(|ui| {
                let open = |mins: u64| now.saturating_add(mins * 60_000);
                if ui.add(primary_button("Ouvrir 15 min")).clicked() {
                    let _ = self.data_tx.send(DataCmd::SetRequestsOpen { until_ms: open(15) });
                }
                if ui.button("Ouvrir 1 h").clicked() {
                    let _ = self.data_tx.send(DataCmd::SetRequestsOpen { until_ms: open(60) });
                }
                if ui
                    .button("Sans limite")
                    .on_hover_text("Déconseillé sur Internet : n'importe qui peut alors voir que ce port est un host GRX et déposer une demande.")
                    .clicked()
                {
                    let _ = self.data_tx.send(DataCmd::SetRequestsOpen { until_ms: REQUESTS_ALWAYS_OPEN });
                }
                if ui.add_enabled(st.open_until_ms != 0, egui::Button::new("Fermer")).clicked() {
                    let _ = self.data_tx.send(DataCmd::SetRequestsOpen { until_ms: 0 });
                }
            });
            if st.is_open(now) && !running {
                ui.label(colored("Le host n'est pas démarré : aucune demande ne peut arriver.", WARN));
            }
            ui.label(small_muted(
                "Le client choisit « Demander l'accès » (onglet Client) et tape l'adresse de ce PC : IP locale:port sur le même réseau, \
                 adresse publique:port depuis Internet. Avant d'accepter, comparez l'empreinte de la demande avec celle affichée chez le client : \
                 c'est ce qui garantit que vous acceptez le bon appareil.",
            ));

            let pending: Vec<RequestRow> = st.rows.iter().filter(|r| !r.denied).cloned().collect();
            let denied: Vec<RequestRow> = st.rows.iter().filter(|r| r.denied).cloned().collect();
            if pending.is_empty() {
                ui.label(muted("Aucune demande en attente."));
            } else {
                let mut approve = None;
                let mut deny = None;
                egui::Grid::new("requests").striped(true).num_columns(5).min_col_width(60.0).show(ui, |ui| {
                    for h in ["Nom annoncé", "Adresse", "Empreinte du client", "Reçue", ""] {
                        ui.label(small_muted(h));
                    }
                    ui.end_row();
                    for r in &pending {
                        ui.label(RichText::new(&r.name).strong());
                        ui.label(RichText::new(&r.addr).monospace().size(13.0));
                        ui.label(RichText::new(&r.fingerprint).monospace().size(13.0));
                        ui.label(format_date_local(r.first_ms));
                        ui.horizontal(|ui| {
                            if ui.add(primary_button("Accepter…")).clicked() {
                                approve = Some(r.clone());
                            }
                            if ui.add(danger_button("Refuser")).clicked() {
                                deny = Some(r.clone());
                            }
                        });
                        ui.end_row();
                    }
                });
                if let Some(r) = approve {
                    self.access.approve = Some(ApproveDialog { row: r, role: Role::Full });
                }
                if let Some(r) = deny {
                    let _ = self.data_tx.send(DataCmd::DenyRequest { fingerprint: r.fingerprint, name: r.name });
                }
            }
            if !denied.is_empty() {
                egui::CollapsingHeader::new(format!("Refusées récemment ({})", denied.len())).default_open(false).show(ui, |ui| {
                    for r in &denied {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(muted(format!("{} — {} — {}", r.name, r.addr, r.fingerprint)));
                            if ui.small_button("Oublier").on_hover_text("Le client pourra redemander.").clicked() {
                                let _ = self.data_tx.send(DataCmd::ForgetRequest { fingerprint: r.fingerprint.clone() });
                            }
                        });
                    }
                    ui.label(small_muted("Un refus est retenu 1 h : le client reçoit « refusé » au lieu de redemander."));
                });
            }
            ui.label(small_muted("Pour retirer l'accès à un client déjà accepté : « Révoquer » dans la liste des clients autorisés."));
        });
    }

    pub(super) fn approve_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.access.approve.clone() else { return };
        let mut close = false;
        let mut confirm = false;
        let m = egui::Modal::new(egui::Id::unique("approve-request")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading("Accepter cette demande ?");
            ui.label(format!("« {} » depuis {}", d.row.name, d.row.addr));
            ui.label(muted("Empreinte du client :"));
            ui.label(RichText::new(&d.row.fingerprint).monospace().size(18.0).strong());
            ui.label(colored(
                "Vérifiez qu'elle est IDENTIQUE à celle affichée sur l'écran du client (« Empreinte de ce client »). \
                 Le nom annoncé, lui, n'est pas vérifié.",
                WARN,
            ));
            ui.horizontal(|ui| {
                ui.label("Rôle :");
                egui::ComboBox::from_id_salt("approve-role")
                    .selected_text(match d.role {
                        Role::ViewOnly => "Lecture seule",
                        _ => "Complet",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.role, Role::Full, "Complet");
                        ui.selectable_value(&mut d.role, Role::ViewOnly, "Lecture seule");
                    });
            });
            ui.label(small_muted("« Contrôle total » se donne ensuite dans la liste des clients autorisés."));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(primary_button("Accepter")).clicked() {
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
            let _ = self.data_tx.send(DataCmd::ApproveRequest { fingerprint: d.row.fingerprint, name: d.row.name, role: d.role });
            self.access.approve = None;
        } else if close {
            self.access.approve = None;
        } else {
            self.access.approve = Some(d);
        }
    }

    /// Bouton et résultats du test de joignabilité (dans la carte de statut, host démarré).
    pub(super) fn reachability(&mut self, ui: &mut egui::Ui, port: u16, public: Option<String>) {
        ui.horizontal_wrapped(|ui| {
            let b = ui
                .add_enabled(!self.access.reach_pending, egui::Button::new(if self.access.reach_pending { "Test en cours…" } else { "Tester la joignabilité" }))
                .on_hover_text(
                    "Envoie au host une sonde que lui seul reconnaît (dérivée de sa clé privée) : sur ce PC, sur l'IP locale et sur \
                     l'adresse publique. Aucun service externe n'est contacté.",
                );
            if b.clicked() {
                self.access.reach_pending = true;
                self.access.reach = None;
                let _ = self.data_tx.send(DataCmd::Reachability { port, public: public.clone() });
            }
        });
        if let Some(lines) = &self.access.reach {
            for l in lines {
                let (c, txt) = match &l.result {
                    ReachResult::Ok(d) => (OK, format!("répond ({} ms)", d.as_millis().max(1))),
                    ReachResult::WrongHost => (ERR, "un AUTRE host répond à cette adresse".to_string()),
                    ReachResult::NoReply => (WARN, "pas de réponse".to_string()),
                    ReachResult::Error(e) => (ERR, e.clone()),
                };
                ui.horizontal_wrapped(|ui| {
                    dot(ui, c);
                    ui.label(format!("{} ({}) :", l.label, l.addr));
                    ui.label(colored(txt, c));
                });
                if !matches!(l.result, ReachResult::Ok(_)) {
                    ui.label(small_muted(&l.hint));
                }
            }
        }
    }

    // ───────────────────────────── client ─────────────────────────────

    pub(super) fn request_access_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Demander l'accès (sans lien)", |ui| {
            if let Some(r) = self.access.req.clone() {
                self.request_progress(ui, &r);
                return;
            }
            ui.label(muted(
                "Tapez l'adresse du host (IP:port). Son propriétaire doit avoir ouvert les demandes (onglet Host) puis valider la vôtre.",
            ));
            let existing: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
            let addr_check = validate_addr(&self.access.req_addr);
            egui::Grid::new("req-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Adresse du host");
                let r = ui.add(
                    TextEdit::singleline(&mut self.access.req_addr)
                        .hint_text(format!("192.168.1.20:{DEFAULT_PORT}  ou  mon-pc.duckdns.org:443"))
                        .desired_width(300.0)
                        .font(egui::TextStyle::Monospace),
                );
                if r.changed() && !self.access.req_name_touched {
                    if let Ok(a) = validate_addr(&self.access.req_addr) {
                        self.access.req_name = logic::suggest_name(&a, &existing);
                    }
                }
                ui.end_row();
                ui.label("Nom du host");
                if ui.add(TextEdit::singleline(&mut self.access.req_name).desired_width(240.0)).changed() {
                    self.access.req_name_touched = true;
                }
                ui.end_row();
            });
            let name_check = validate_name(&self.access.req_name, &existing);
            if !self.access.req_addr.trim().is_empty() {
                if let Err(m) = &addr_check {
                    ui.label(colored(m, ERR));
                }
            }
            if !self.access.req_name.trim().is_empty() {
                if let Err(m) = &name_check {
                    ui.label(colored(m, ERR));
                }
            }
            let ok = addr_check.is_ok() && name_check.is_ok();
            if ui
                .add_enabled(ok, primary_button("Envoyer la demande"))
                .on_hover_text("Lance grx-client request : la demande est relancée toutes les 2 s pendant 5 min au plus.")
                .clicked()
            {
                if let (Ok(addr), Ok(name)) = (addr_check, name_check) {
                    let args = vec![
                        "--data-dir".to_string(),
                        self.paths.client_dir.to_string_lossy().into_owned(),
                        "request".into(),
                        "-".into(),
                        "--save-as".into(),
                        name.clone(),
                    ];
                    self.save_settings();
                    let id = self.sessions.launch(self.paths.client_exe.clone(), args, addr.clone(), name.clone(), self.sink.clone());
                    self.access.req = Some(AccessReqUi::new(id, addr, name));
                }
            }
            ui.label(small_muted(
                "Moins sûr qu'une invitation : la clé du host est reçue telle quelle. Comparez son empreinte avec celle affichée sur le host.",
            ));
        });
    }

    fn request_progress(&mut self, ui: &mut egui::Ui, r: &AccessReqUi) {
        ui.label(format!("Demande à {} (« {} »)", r.addr, r.name));
        let (c, txt) = match r.phase {
            ReqPhase::Contacting => (MUTED, "Contact du host…"),
            ReqPhase::Sent => (MUTED, "Clé du host reçue, demande envoyée…"),
            ReqPhase::Pending => (WARN, "En attente de validation sur le host…"),
            ReqPhase::NoReply => (WARN, "Le host ne répond plus (demandes refermées ?) : nouvelles tentatives…"),
            ReqPhase::Accepted => (OK, "Accès accordé, connexion…"),
            ReqPhase::Denied => (ERR, "Demande refusée par le host."),
        };
        ui.horizontal_wrapped(|ui| {
            dot(ui, c);
            ui.label(colored(txt, c));
        });
        if let Some(fp) = &r.host_fp {
            egui::Grid::new("req-fps").num_columns(2).show(ui, |ui| {
                ui.label(muted("Empreinte du host"));
                ui.label(RichText::new(fp).monospace().size(17.0).strong());
                ui.end_row();
                if let Some(Ok(me)) = &self.client_fp {
                    ui.label(muted("Votre empreinte"));
                    ui.label(RichText::new(me).monospace().size(17.0).strong());
                    ui.end_row();
                }
            });
            ui.label(colored(
                "Le propriétaire du host doit voir VOTRE empreinte dans sa demande, et l'empreinte du host ci-dessus doit être celle de son \
                 onglet Host. Si l'une diffère : annulez (quelqu'un s'intercale).",
                WARN,
            ));
        }
        ui.horizontal(|ui| {
            if r.phase == ReqPhase::Denied {
                if ui.button("Fermer").clicked() {
                    self.access.req = None;
                }
            } else if ui.add_enabled(!r.cancelled, danger_button("Annuler")).clicked() {
                self.sessions.cancel(r.id);
                if let Some(q) = self.access.req.as_mut() {
                    q.cancelled = true;
                }
            }
        });
        if matches!(r.phase, ReqPhase::Contacting | ReqPhase::Sent | ReqPhase::Pending | ReqPhase::NoReply) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }
}
