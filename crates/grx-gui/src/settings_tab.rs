//! Onglet « Paramètres » : tous les réglages de lancement (client et host), le codec, les mises à
//! jour. Une seule source de vérité : `GuiSettings` (gui.json) ; les cases des onglets Client et
//! Host lisent et écrivent les mêmes champs. Les arguments sont construits par `launch_opts.rs`.
//!
//! Ajouter un réglage : champ + argument + test dans `launch_opts.rs`, puis un widget dans la
//! section voulue ci-dessous (et son retour au défaut dans le bouton « Valeurs par défaut »).
//! Options pas encore exposées par les exécutables (ex. « Préférer 10 bits ») : ne rien afficher
//! tant que `grx-host run --help` / `grx-client play --help` ne les proposent pas.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use eframe::egui::{self, RichText, TextEdit};

use super::App;
use crate::launch_opts::{
    codec_warning, encoder_warning, parse_decodable, parse_encoders, ChromaChoice, ClientGamepadAs, ClientPlayOptions, CodecChoice, DecoderChoice,
    EncoderChoice, EncoderRow, HostGamepadType, HostRunOptions,
};
use crate::theme::{card, colored, muted, small_muted, ERR, WARN};
use crate::update::{self, MenuAction, UpdatePrefs};

/// Sonde des décodeurs vidéo de ce PC (`grx-client decoders`, sans fenêtre), lancée une fois.
#[derive(Default)]
pub struct DecoderProbe {
    state: ProbeState,
}

#[derive(Default)]
enum ProbeState {
    #[default]
    NotStarted,
    Running(Receiver<Option<Vec<CodecChoice>>>),
    Done(Option<Vec<CodecChoice>>),
}

impl DecoderProbe {
    fn ensure(&mut self, exe: PathBuf, data_dir: PathBuf, ctx: &egui::Context) {
        if !matches!(self.state, ProbeState::NotStarted) {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--data-dir").arg(data_dir).arg("decoders").stdin(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let r = cmd
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| parse_decodable(&String::from_utf8_lossy(&o.stdout)));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.state = ProbeState::Running(rx);
    }

    fn poll(&mut self) {
        if let ProbeState::Running(rx) = &self.state {
            match rx.try_recv() {
                Ok(r) => self.state = ProbeState::Done(r),
                Err(TryRecvError::Disconnected) => self.state = ProbeState::Done(None),
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    /// Codecs décodables par ce PC (`None` : sonde en cours ou impossible).
    fn decodable(&self) -> Option<&[CodecChoice]> {
        match &self.state {
            ProbeState::Done(Some(v)) => Some(v),
            _ => None,
        }
    }
}

/// Sonde des encodeurs de ce PC par test réel (`grx-host encoders --json`, sans fenêtre) : relit le
/// cache du host (instantané) ou teste chaque combinaison (quelques secondes).
#[derive(Default)]
pub struct EncoderProbe {
    state: EncState,
}

#[derive(Default)]
enum EncState {
    #[default]
    NotStarted,
    Running(Receiver<Option<Vec<EncoderRow>>>),
    Done(Option<Vec<EncoderRow>>),
}

impl EncoderProbe {
    fn start(&mut self, exe: PathBuf, data_dir: PathBuf, refresh: bool, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--data-dir").arg(data_dir).arg("encoders").arg("--json").stdin(std::process::Stdio::null());
            if refresh {
                cmd.arg("--refresh");
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let r = cmd
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| parse_encoders(&String::from_utf8_lossy(&o.stdout)));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.state = EncState::Running(rx);
    }

    fn poll(&mut self) {
        if let EncState::Running(rx) = &self.state {
            match rx.try_recv() {
                Ok(r) => self.state = EncState::Done(r),
                Err(TryRecvError::Disconnected) => self.state = EncState::Done(None),
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    fn rows(&self) -> Option<&[EncoderRow]> {
        match &self.state {
            EncState::Done(Some(v)) => Some(v),
            _ => None,
        }
    }
}

/// En-tête de section avec bouton « Valeurs par défaut » à droite. Renvoie `true` si cliqué.
fn section_header(ui: &mut egui::Ui, hint: &str) -> bool {
    let mut reset = false;
    ui.horizontal(|ui| {
        ui.label(small_muted(hint));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            reset = ui.small_button("Valeurs par défaut").on_hover_text("Remet les réglages de cette section à leur valeur par défaut").clicked();
        });
    });
    reset
}

fn combo<T: Copy + PartialEq>(ui: &mut egui::Ui, id: &str, value: &mut T, all: &[T], label: fn(T) -> &'static str) {
    egui::ComboBox::from_id_salt(id).width(280.0).selected_text(label(*value)).show_ui(ui, |ui| {
        for &c in all {
            ui.selectable_value(value, c, label(c));
        }
    });
}

/// Petits badges « réglage forcé » (onglets Client et Host) : évite les surprises.
pub fn forced_badges(ui: &mut egui::Ui, items: &[String]) -> bool {
    if items.is_empty() {
        return false;
    }
    let mut open = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(small_muted("Réglages forcés :"));
        for it in items {
            let b = egui::Button::new(RichText::new(it).small().color(WARN)).corner_radius(8.0).stroke(egui::Stroke::new(1.0, WARN));
            open |= ui.add(b).on_hover_text("Modifiable dans l'onglet Paramètres").clicked();
        }
    });
    open
}

impl App {
    fn ensure_decoder_probe(&mut self, ctx: &egui::Context) {
        self.decoders.ensure(self.paths.client_exe.clone(), self.paths.client_dir.clone(), ctx);
        self.decoders.poll();
    }

    fn ensure_encoder_probe(&mut self, ctx: &egui::Context) {
        if matches!(self.encoders.state, EncState::NotStarted) {
            self.encoders.start(self.paths.host_exe.clone(), self.paths.host_dir.clone(), false, ctx);
        }
        self.encoders.poll();
    }

    /// Avertissement si le codec demandé par ce client n'est pas décodable ici.
    pub(super) fn client_codec_warning(&mut self, ctx: &egui::Context) -> Option<String> {
        if self.settings.client.codec == CodecChoice::Auto {
            return None;
        }
        self.ensure_decoder_probe(ctx);
        codec_warning(self.settings.client.codec, self.decoders.decodable())
    }

    pub(super) fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let before = self.settings.clone();
        let ctx = ui.ctx().clone();
        self.ensure_decoder_probe(&ctx);
        ui.label(muted(
            "Tous les réglages appliqués aux lancements faits depuis cette fenêtre (« Connecter », « Démarrer le host »). \
             Enregistrés automatiquement.",
        ));
        self.codec_section(ui, &ctx);
        self.client_section(ui);
        self.host_section(ui);
        self.updates_section(ui);
        if self.settings != before {
            self.save_settings();
        }
    }

    fn codec_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        card(ui, "Codec vidéo", |ui| {
            if section_header(ui, "Automatique : ce PC annonce ses décodeurs et le host choisit le meilleur codec commun (HEVC, sinon H.264). Recommandé.") {
                let (c, h) = (ClientPlayOptions::default(), HostRunOptions::default());
                self.settings.client.codec = c.codec;
                self.settings.client.decoder = c.decoder;
                self.settings.host.codec = h.codec;
                self.settings.host.encoder = h.encoder;
                self.settings.host.chroma = h.chroma;
            }
            let warn = self.client_codec_warning(ctx);
            self.ensure_encoder_probe(ctx);
            let h = &self.settings.host;
            let enc_warn = encoder_warning(h.encoder, h.chroma, h.codec, self.encoders.rows());
            let host_locked = self.host.running;
            egui::Grid::new("set-codec").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Codec demandé (ce PC client)")
                    .on_hover_text("Codec demandé au host lors de « Connecter » (play --codec)");
                ui.horizontal(|ui| {
                    combo(ui, "set-ccodec", &mut self.settings.client.codec, &CodecChoice::ALL, CodecChoice::label);
                    if let Some(w) = &warn {
                        ui.label(colored(format!("⚠ {w}"), WARN));
                    }
                });
                ui.end_row();
                ui.label("Décodeur (ce PC client)").on_hover_text("play --decoder : matériel (GPU) ou logiciel (CPU)");
                combo(ui, "set-dec", &mut self.settings.client.decoder, &DecoderChoice::ALL, DecoderChoice::label);
                ui.end_row();
                ui.label("Codec préféré (ce host)")
                    .on_hover_text("run --codec : si le client ne sait pas le décoder, le host bascule automatiquement");
                ui.add_enabled_ui(!host_locked, |ui| {
                    combo(ui, "set-hcodec", &mut self.settings.host.codec, &CodecChoice::ALL, CodecChoice::label)
                });
                ui.end_row();
                ui.label("Encodeur (ce host)").on_hover_text("run --encoder : GPU utilisé pour l'encodage");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!host_locked, |ui| {
                        combo(ui, "set-enc", &mut self.settings.host.encoder, &EncoderChoice::ALL, EncoderChoice::label)
                    });
                    if let Some(w) = &enc_warn {
                        ui.label(colored(format!("⚠ {w}"), WARN));
                    }
                });
                ui.end_row();
                ui.label("Couleur (ce host)").on_hover_text("run --chroma : 4:4:4 = texte plus net (NVIDIA HEVC, client compatible)");
                ui.add_enabled_ui(!host_locked, |ui| {
                    combo(ui, "set-chroma", &mut self.settings.host.chroma, &ChromaChoice::ALL, ChromaChoice::label)
                });
                ui.end_row();
            });
            ui.horizontal_wrapped(|ui| {
                let text = match &self.encoders.state {
                    EncState::NotStarted | EncState::Running(_) => "Encodeurs de ce PC (test réel) : sondage en cours…".to_string(),
                    EncState::Done(None) => "Encodeurs de ce PC : sondage impossible (grx-host introuvable ?).".to_string(),
                    EncState::Done(Some(v)) => {
                        let ok: Vec<String> = v.iter().filter(|r| r.ok).map(|r| r.summary()).collect();
                        if ok.is_empty() {
                            "Encodeurs de ce PC (test réel) : aucun encodeur n'a passé le test.".to_string()
                        } else {
                            format!("Encodeurs de ce PC (test réel) : {}", ok.join(", "))
                        }
                    }
                };
                let failed: Vec<String> = self
                    .encoders
                    .rows()
                    .map(|v| v.iter().filter(|r| !r.ok).map(|r| format!("{} : {}", r.summary(), r.reason)).collect())
                    .unwrap_or_default();
                let l = ui.label(small_muted(text));
                if !failed.is_empty() {
                    l.on_hover_text(format!("Combinaisons refusées :
{}", failed.join("
")));
                }
                let busy = matches!(self.encoders.state, EncState::Running(_));
                if ui.add_enabled(!busy, egui::Button::new("Re-tester").small()).on_hover_text("grx-host encoders --refresh").clicked() {
                    self.encoders.start(self.paths.host_exe.clone(), self.paths.host_dir.clone(), true, ctx);
                }
            });
            let probe = match &self.decoders.state {
                ProbeState::NotStarted | ProbeState::Running(_) => "Décodeurs de ce PC : sondage en cours…".to_string(),
                ProbeState::Done(Some(v)) if v.is_empty() => "Décodeurs de ce PC : aucun décodeur vidéo utilisable.".to_string(),
                ProbeState::Done(Some(v)) => format!(
                    "Décodeurs de ce PC : {}",
                    v.iter().map(|c| c.label()).collect::<Vec<_>>().join(", ")
                ),
                ProbeState::Done(None) => "Décodeurs de ce PC : sondage impossible (grx-client introuvable ?).".to_string(),
            };
            ui.label(small_muted(probe));
            if host_locked {
                ui.label(small_muted("Réglages du host : arrêtez le host pour les modifier."));
            }
        });
    }

    fn client_section(&mut self, ui: &mut egui::Ui) {
        card(ui, "Client (lecture)", |ui| {
            let c = &mut self.settings.client;
            if section_header(ui, "Appliqué à « Connecter » et « Appairer et connecter ».") {
                let d = ClientPlayOptions::default();
                *c = ClientPlayOptions { codec: c.codec, decoder: c.decoder, ..d };
            }
            ui.checkbox(&mut c.fullscreen, "Plein écran au démarrage").on_hover_text("--fullscreen (F11 bascule)");
            ui.checkbox(&mut c.vsync, "Synchronisation verticale (VSync)").on_hover_text("--vsync : moins de déchirure, un peu plus de latence");
            let mut send_input = !c.no_input;
            if ui.checkbox(&mut send_input, "Envoyer clavier et souris au host").on_hover_text("Décoché : --no-input").changed() {
                c.no_input = !send_input;
            }
            ui.add_enabled_ui(!c.no_input, |ui| {
                let mut pads = !c.no_gamepad;
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut pads, "Envoyer les manettes").on_hover_text("Décoché : --no-gamepad").changed() {
                        c.no_gamepad = !pads;
                    }
                    ui.add_enabled_ui(pads, |ui| {
                        ui.label("Type demandé :");
                        combo(ui, "set-padas", &mut c.gamepad_as, &ClientGamepadAs::ALL, ClientGamepadAs::label);
                    });
                });
            });
            ui.checkbox(&mut c.local_cursor, "Curseur Windows local (au lieu du curseur du host)").on_hover_text("--local-cursor");
            let mut audio = !c.no_audio;
            ui.horizontal(|ui| {
                if ui.checkbox(&mut audio, "Jouer le son").on_hover_text("Décoché : --no-audio").changed() {
                    c.no_audio = !audio;
                }
                ui.add_enabled_ui(audio, |ui| {
                    ui.label("Volume :");
                    ui.add(egui::Slider::new(&mut c.volume, 0..=100).suffix(" %"));
                });
            });
            ui.checkbox(&mut c.stats, "Statistiques (FPS, débit, latence) dans le titre").on_hover_text("--stats");
        });
    }

    fn host_section(&mut self, ui: &mut egui::Ui) {
        let running = self.host.running;
        card(ui, "Host (diffusion)", |ui| {
            let o = &mut self.settings.host;
            if section_header(ui, "Appliqué à « Démarrer le host ».") && !running {
                let d = HostRunOptions::default();
                *o = HostRunOptions { codec: o.codec, encoder: o.encoder, chroma: o.chroma, ..d };
            }
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("set-host").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Écoute (ip:port)").on_hover_text("--bind : [::]:443 = IPv4 + IPv6");
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(&mut o.bind).desired_width(170.0).font(egui::TextStyle::Monospace));
                        if ui.small_button("IPv4").on_hover_text("0.0.0.0 : IPv4 seulement").clicked() {
                            o.bind = rebind(&o.bind, "0.0.0.0");
                        }
                        if ui.small_button("IPv4 + IPv6").on_hover_text("[::] : double pile").clicked() {
                            o.bind = rebind(&o.bind, "[::]");
                        }
                        port_presets(ui, &mut o.bind);
                    });
                    ui.end_row();
                    ui.label("Images / seconde (max)").on_hover_text("--fps");
                    ui.add(egui::DragValue::new(&mut o.fps).range(1..=240).speed(0.5));
                    ui.end_row();
                    ui.label("Débit maximal").on_hover_text("--max-bitrate (Mb/s)");
                    ui.add(egui::DragValue::new(&mut o.max_bitrate).range(1..=500).suffix(" Mb/s"));
                    ui.end_row();
                    ui.label("Débit de départ").on_hover_text("--start-bitrate (Mb/s)");
                    ui.add(egui::DragValue::new(&mut o.start_bitrate).range(1..=500).suffix(" Mb/s"));
                    ui.end_row();
                    ui.label("Écran de départ").on_hover_text("--display : 1, 2… de gauche à droite ; le client peut en changer depuis son menu");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut o.display).range(0..=16));
                        ui.label(small_muted(if o.display == 0 { "écran principal" } else { "" }));
                    });
                    ui.end_row();
                    ui.label("Délai d'inactivité").on_hover_text("--idle-timeout : fermeture de session sans nouvelles du client");
                    ui.add(egui::DragValue::new(&mut o.idle_timeout).range(1..=3600).suffix(" s"));
                    ui.end_row();
                });
                ui.checkbox(&mut o.fill, "Cadence constante (ré-encode l'image si l'écran est statique)").on_hover_text("--fill");
                ui.checkbox(&mut o.no_audio, "Ne pas diffuser le son").on_hover_text("--no-audio");
                ui.label(RichText::new("Entrées autorisées").strong());
                ui.checkbox(&mut o.no_input, "Aucune entrée (test sans risque)").on_hover_text("--no-input");
                ui.add_enabled_ui(!o.no_input, |ui| {
                    ui.horizontal(|ui| {
                        let mut k = !o.no_keyboard;
                        if ui.checkbox(&mut k, "Clavier").changed() {
                            o.no_keyboard = !k;
                        }
                        let mut m = !o.no_mouse;
                        if ui.checkbox(&mut m, "Souris").changed() {
                            o.no_mouse = !m;
                        }
                        let mut g = !o.no_gamepad;
                        if ui.checkbox(&mut g, "Manettes").changed() {
                            o.no_gamepad = !g;
                        }
                        ui.add_enabled_ui(g, |ui| {
                            ui.label("Type :").on_hover_text("--gamepad-type : manette virtuelle créée sur ce PC");
                            combo(ui, "set-padtype", &mut o.gamepad_type, &HostGamepadType::ALL, HostGamepadType::label);
                        });
                    });
                    ui.checkbox(&mut o.block_win_key, "Bloquer la touche Windows du client").on_hover_text("--block-win-key");
                });
                ui.label(RichText::new("Réseau").strong());
                ui.checkbox(&mut o.upnp, "Ouvrir le port automatiquement sur la box (UPnP / PCP / NAT-PMP)").on_hover_text("Décoché : --no-upnp");
                ui.add_enabled_ui(o.upnp, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Port externe sur la box :").on_hover_text("--external-port");
                        ui.add(egui::DragValue::new(&mut o.external_port).range(0..=65535));
                        ui.label(small_muted(if o.external_port == 0 { "(0 = même que le port d'écoute)" } else { "" }));
                    });
                });
            });
            if let Err(e) = o.validate() {
                ui.label(colored(e, ERR));
            }
            if running {
                ui.label(small_muted("Arrêtez le host (onglet Host) pour modifier ces réglages."));
            }
        });
        self.elevation_card(ui, running);
    }

    fn updates_section(&mut self, ui: &mut egui::Ui) {
        let mut action = MenuAction::None;
        card(ui, "Mises à jour", |ui| {
            if section_header(ui, &format!("Version installée : {}", update::current_version())) {
                self.settings.update.auto_check = UpdatePrefs::default().auto_check;
            }
            action = self.update.menu(ui, &mut self.settings.update);
        });
        if matches!(action, MenuAction::CheckNow) {
            self.check_updates(true);
        }
    }
}

/// Bouton « Port 443 » (défaut) : remet le port d'écoute à 443 en gardant l'IP.
pub fn port_presets(ui: &mut egui::Ui, bind: &mut String) {
    if ui
        .small_button("Port 443")
        .on_hover_text(
            "UDP 443 : le port de HTTP/3 (QUIC), souvent autorisé par les réseaux filtrés (Wi-Fi public, entreprise). Ne passe pas un réseau qui n'autorise que TCP 443 ou impose un proxy. Pensez à la règle du pare-feu Windows pour ce port (bouton « Voir la commande ») et à régénérer les invitations.",
        )
        .clicked()
    {
        *bind = crate::logic::rebind_port(bind, crate::logic::DEFAULT_PORT);
    }
}

/// Remplace l'IP de `bind` en gardant le port (443 si illisible).
fn rebind(bind: &str, ip: &str) -> String {
    let port = bind.trim().rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(crate::logic::DEFAULT_PORT);
    format!("{ip}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebind_keeps_port() {
        assert_eq!(rebind("0.0.0.0:9000", "[::]"), "[::]:9000");
        assert_eq!(rebind("[::]:7778", "0.0.0.0"), "0.0.0.0:7778");
        assert_eq!(rebind("n'importe quoi", "[::]"), "[::]:443");
    }
}
