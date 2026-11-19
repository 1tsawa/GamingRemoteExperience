//! Préférences de l'interface (`gui.json` dans le dossier de données du host).
//! Ne contient JAMAIS de secret : ni jeton, ni lien d'invitation, ni clé.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::logic::{ClientPlayOptions, HostRunOptions, Ttl};
use crate::update::UpdatePrefs;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct GuiSettings {
    pub public_addr: String,
    /// L'adresse a été remplie automatiquement (status.json / box) : elle peut être remplacée
    /// par une nouvelle adresse automatique. Une saisie manuelle la remet à `false`.
    pub public_addr_auto: bool,
    pub ttl: Ttl,
    pub host: HostRunOptions,
    pub client: ClientPlayOptions,
    /// Recherche de mises à jour (activée par défaut, au plus une fois par jour).
    pub update: UpdatePrefs,
    /// Démarrer le host en mode « Contrôle total » (host élevé via la tâche planifiée).
    pub host_elevated: bool,
    /// Lancer aussi le host élevé à l'ouverture de session (option de la tâche).
    pub host_at_logon: bool,
}

pub fn settings_path(host_dir: &Path) -> PathBuf {
    host_dir.join("gui.json")
}

/// Fichier absent ou illisible = réglages par défaut (jamais bloquant). L'ancien port par défaut
/// (7777) est migré vers 443 (voir [`HostRunOptions::migrate_old_port`]) ; la migration est écrite
/// au prochain enregistrement.
pub fn load(path: &Path) -> GuiSettings {
    let mut s: GuiSettings = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    s.host.migrate_old_port();
    s
}

pub fn save(path: &Path, s: &GuiSettings) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    grx_core::util::write_atomic(path, &json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults_and_unknown_files() {
        let dir = std::env::temp_dir().join(format!("grx-gui-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = settings_path(&dir);
        assert_eq!(load(&p), GuiSettings::default());
        let s = GuiSettings {
            public_addr: "1.2.3.4:443".into(),
            public_addr_auto: true,
            ttl: Ttl::Hour24,
            host: HostRunOptions { fps: 30, no_input: true, ..Default::default() },
            client: ClientPlayOptions { fullscreen: true, ..Default::default() },
            update: UpdatePrefs { auto_check: false, last_check_ms: 42 },
            host_elevated: true,
            host_at_logon: false,
        };
        save(&p, &s).unwrap();
        assert_eq!(load(&p), s);
        std::fs::write(&p, b"{pas du json").unwrap();
        assert_eq!(load(&p), GuiSettings::default());
        std::fs::write(&p, br#"{"public_addr":"x:1"}"#).unwrap();
        assert_eq!(load(&p).public_addr, "x:1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_gui_json_gets_new_settings_defaults() {
        // gui.json d'une version 0.2.1 : aucun des nouveaux réglages de l'onglet Paramètres.
        let old = br#"{"public_addr":"x:1","host":{"bind":"0.0.0.0:7777","codec":"Hevc","fps":30,"upnp":true},
            "client":{"no_input":false,"no_audio":true,"fullscreen":true},"update":{"auto_check":false}}"#;
        let s: GuiSettings = serde_json::from_slice(old).unwrap();
        assert_eq!(s.host.codec, crate::logic::CodecChoice::Hevc);
        assert_eq!((s.host.fps, s.host.max_bitrate, s.host.idle_timeout, s.host.display), (30, 30, 5, 0));
        assert!(s.client.fullscreen && s.client.no_audio && s.client.volume == 80);
        assert_eq!(s.client.codec, crate::logic::CodecChoice::Auto);
        assert!(!s.update.auto_check);
    }

    #[test]
    fn load_migrates_old_port_then_save_persists() {
        let dir = std::env::temp_dir().join(format!("grx-gui-settings-port-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = settings_path(&dir);
        std::fs::write(&p, br#"{"host":{"bind":"192.168.1.20:7777","external_port":7777,"fps":30}}"#).unwrap();
        let s = load(&p);
        assert_eq!((s.host.bind.as_str(), s.host.external_port, s.host.fps), ("192.168.1.20:443", 0, 30));
        save(&p, &s).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(raw["host"]["bind"], "192.168.1.20:443");
        assert_eq!(raw["host"]["external_port"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
