//! Fenêtre « Modifier » d'un host mémorisé (onglet Client) : nom et liste ORDONNÉE d'adresses
//! (réseau local, Internet, ancien et nouveau port…). L'identité du host (sa clé, son empreinte)
//! ne change pas : pas besoin de réappairer. Validation pure dans [`validate_edit`].

use eframe::egui::{self, RichText, TextEdit};

use super::App;
use crate::backend::{DataCmd, ProfileRow};
use crate::logic::{validate_addr, validate_name, DEFAULT_PORT};
use crate::theme::{colored, muted, primary_button, small_muted, ERR, OK};

/// État de la fenêtre (une seule à la fois).
#[derive(Clone, Debug)]
pub struct EditHostDialog {
    /// Nom actuel (clé de la modification).
    pub old: String,
    pub fingerprint: String,
    pub name: String,
    /// Adresses saisies, dans l'ordre d'essai.
    pub addrs: Vec<String>,
    /// Champ « ajouter une adresse ».
    pub new_addr: String,
}

impl EditHostDialog {
    pub fn new(p: &ProfileRow) -> Self {
        Self {
            old: p.name.clone(),
            fingerprint: p.fingerprint.clone(),
            name: p.name.clone(),
            addrs: p.addrs.clone(),
            new_addr: String::new(),
        }
    }
}

/// Valide nom et adresses : nom libre (hors noms pris par les AUTRES hosts), au moins une adresse,
/// chacune `ip:port`, `[ipv6]:port` ou `nom:port` (port 1-65535), sans doublon, 8 au plus.
/// Renvoie le nom et les adresses nettoyés, ou le premier message d'erreur.
pub fn validate_edit(name: &str, addrs: &[String], others: &[String]) -> Result<(String, Vec<String>), String> {
    let name = validate_name(name, others)?;
    let mut out: Vec<String> = Vec::new();
    for (i, a) in addrs.iter().enumerate() {
        let v = validate_addr(a).map_err(|e| format!("Adresse n° {} : {e}", i + 1))?;
        if out.iter().any(|o| o.eq_ignore_ascii_case(&v)) {
            return Err(format!("Adresse n° {} : « {v} » est déjà dans la liste.", i + 1));
        }
        out.push(v);
    }
    if out.is_empty() {
        return Err("Gardez au moins une adresse.".into());
    }
    if out.len() > grx_core::profiles::MAX_ADDRS {
        return Err(format!("{} adresses au plus.", grx_core::profiles::MAX_ADDRS));
    }
    Ok((name, out))
}

/// Texte de la colonne « Adresse » : adresse principale, et « +N » s'il y en a d'autres.
pub fn addr_summary(addrs: &[String]) -> String {
    match addrs {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first}  +{}", rest.len()),
    }
}

impl App {
    pub(super) fn edit_host_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.edit_host.clone() else { return };
        let others: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).filter(|n| *n != d.old).collect();
        let check = validate_edit(&d.name, &d.addrs, &others);
        let mut close = false;
        let mut save = false;
        let m = egui::Modal::new(egui::Id::unique("edit-host")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(format!("Modifier « {} »", d.old));
            ui.horizontal(|ui| {
                ui.label(muted("Empreinte du host :"));
                ui.label(RichText::new(&d.fingerprint).monospace());
            });
            ui.label(colored(
                "L'identité du host (son empreinte) ne change pas : pas besoin de réappairer. Seul le vrai host peut répondre, \
                 quelle que soit l'adresse.",
                OK,
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Nom");
                ui.add(TextEdit::singleline(&mut d.name).desired_width(260.0));
            });
            ui.add_space(4.0);
            ui.label("Adresses, essayées dans l'ordre (la première qui répond sert) :");
            let n = d.addrs.len();
            let mut action: Option<(usize, i8)> = None; // (ligne, -1 monter / +1 descendre / 0 supprimer)
            egui::Grid::new("edit-host-addrs").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
                for i in 0..n {
                    ui.label(small_muted(format!("{}.", i + 1)));
                    let bad = validate_addr(&d.addrs[i]).is_err();
                    let mut t = TextEdit::singleline(&mut d.addrs[i]).desired_width(280.0).font(egui::TextStyle::Monospace);
                    if bad {
                        t = t.text_color(ERR);
                    }
                    ui.add(t);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("⬆").small()).on_hover_text("Essayer plus tôt").clicked() {
                            action = Some((i, -1));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("⬇").small()).on_hover_text("Essayer plus tard").clicked() {
                            action = Some((i, 1));
                        }
                        if ui.add_enabled(n > 1, egui::Button::new("Supprimer").small()).clicked() {
                            action = Some((i, 0));
                        }
                    });
                    ui.end_row();
                }
            });
            match action {
                Some((i, 0)) => {
                    d.addrs.remove(i);
                }
                Some((i, -1)) => d.addrs.swap(i, i - 1),
                Some((i, _)) => d.addrs.swap(i, i + 1),
                None => {}
            }
            ui.horizontal(|ui| {
                let r = ui.add(
                    TextEdit::singleline(&mut d.new_addr)
                        .hint_text(format!("192.168.1.20:{DEFAULT_PORT}  ou  monpc.duckdns.org:{DEFAULT_PORT}"))
                        .desired_width(280.0)
                        .font(egui::TextStyle::Monospace),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let add = validate_addr(&d.new_addr);
                if (ui.add_enabled(add.is_ok(), egui::Button::new("Ajouter")).clicked() || enter) && add.is_ok() {
                    if let Ok(a) = add {
                        d.addrs.push(a);
                        d.new_addr.clear();
                    }
                }
            });
            if !d.new_addr.trim().is_empty() {
                if let Err(e) = validate_addr(&d.new_addr) {
                    ui.label(colored(e, ERR));
                }
            }
            ui.label(small_muted(
                "Exemple : l'IP locale pour jouer à la maison (192.168.1.20:443), puis l'adresse publique \
                 ou DuckDNS pour jouer dehors (monpc.duckdns.org:443). Port UDP 443 par défaut.",
            ));
            if let Err(e) = &check {
                ui.label(colored(e, ERR));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(check.is_ok(), primary_button("Enregistrer")).clicked() {
                    save = true;
                }
                if ui.button("Annuler").clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if save {
            if let Ok((name, addrs)) = validate_edit(&d.name, &d.addrs, &others) {
                let _ = self.data_tx.send(DataCmd::EditHost { old: d.old.clone(), new: name, addrs });
            }
            self.edit_host = None;
        } else if close {
            self.edit_host = None;
        } else {
            self.edit_host = Some(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn edit_validation() {
        let others = v(&["bureau"]);
        let (n, a) = validate_edit(" salon ", &v(&[" 192.168.1.20:443", "[2a01:e0a::1]:443", "pc.duckdns.org:7777"]), &others).unwrap();
        assert_eq!(n, "salon");
        assert_eq!(a, v(&["192.168.1.20:443", "[2a01:e0a::1]:443", "pc.duckdns.org:7777"]));
        assert!(validate_edit("bureau", &v(&["a:1"]), &others).unwrap_err().contains("existe déjà"));
        assert!(validate_edit("x", &[], &others).unwrap_err().contains("au moins une"));
        assert!(validate_edit("x", &v(&["a:1", "b:0"]), &others).unwrap_err().starts_with("Adresse n° 2"));
        assert!(validate_edit("x", &v(&["a:1", "A:1"]), &others).unwrap_err().contains("déjà dans la liste"));
        assert!(validate_edit("x", &v(&["0.0.0.0:443"]), &others).is_err());
        let nine: Vec<String> = (0..9).map(|i| format!("h{i}:1")).collect();
        assert!(validate_edit("x", &nine, &others).is_err());
    }

    #[test]
    fn summary() {
        assert_eq!(addr_summary(&v(&["a:1"])), "a:1");
        assert_eq!(addr_summary(&v(&["a:1", "b:2", "c:3"])), "a:1  +2");
        assert_eq!(addr_summary(&[]), "");
    }
}
