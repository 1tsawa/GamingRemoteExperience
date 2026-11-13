use crate::overlay_layout::Frac;
use grx_core::util::write_atomic;
use std::path::Path;

/// Préférences de l'overlay du client, mémorisées entre sessions dans un fichier JSON.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct OverlayPrefs {
    pub version: u32,                 // 1
    pub menu_pos: Option<[f32; 2]>,   // None = position par défaut
    pub stats_pos: Option<[f32; 2]>,
    pub button_pos: Option<[f32; 2]>,
    pub show_stats: bool,             // défaut false
    pub button_enabled: bool,         // défaut true
    pub volume: f32,                  // défaut 0.8
    pub muted: bool,                  // false
    pub vsync: bool,                  // false
    pub capture: bool,                // false
    pub bitrate_cap_mbps: Option<u32>,// None
    pub fps_cap: Option<u32>,         // None
    /// Écran distant choisi (`overlay::display_key`), redemandé à la connexion s'il existe encore.
    pub display: Option<String>,      // None
}

impl Default for OverlayPrefs {
    fn default() -> Self {
        Self {
            version: 1,
            menu_pos: None,
            stats_pos: None,
            button_pos: None,
            show_stats: false,
            button_enabled: true,
            volume: 0.8,
            muted: false,
            vsync: false,
            capture: false,
            bitrate_cap_mbps: None,
            fps_cap: None,
            display: None,
        }
    }
}

impl OverlayPrefs {
    /// Applique les bornes et corrections par défaut aux valeurs non valides ou non finies.
    pub fn sanitized(mut self) -> Self {
        self.version = 1;

        let sanitize_pos = |p: Option<[f32; 2]>| -> Option<[f32; 2]> {
            p.map(|[x, y]| Self::to_arr(Frac { fx: x, fy: y }.sanitized()))
        };
        self.menu_pos = sanitize_pos(self.menu_pos);
        self.stats_pos = sanitize_pos(self.stats_pos);
        self.button_pos = sanitize_pos(self.button_pos);

        if !self.volume.is_finite() {
            self.volume = 0.8;
        } else {
            self.volume = self.volume.clamp(0.0, 1.0);
        }

        self.bitrate_cap_mbps = self.bitrate_cap_mbps.map(|v| v.clamp(1, 1000));
        self.fps_cap = self.fps_cap.map(|v| v.clamp(10, 240));
        // Clé d'écran bornée (fichier modifiable à la main).
        self.display = self.display.filter(|d| !d.is_empty()).map(|d| d.chars().take(128).collect());

        self
    }

    /// Convertit un tableau de coordonnées en `Frac`.
    pub fn pos(p: Option<[f32; 2]>) -> Option<Frac> {
        p.map(|[x, y]| Frac { fx: x, fy: y })
    }

    /// Convertit un `Frac` en tableau de coordonnées.
    pub fn to_arr(f: Frac) -> [f32; 2] {
        [f.fx, f.fy]
    }

    /// Parse un JSON en `OverlayPrefs`. Ignore les champs inconnus, applique les défauts,
    /// retire le BOM UTF-8 initial si présent, et applique `sanitized()`.
    pub fn from_json(text: &str) -> Self {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        serde_json::from_str::<OverlayPrefs>(text).unwrap_or_default().sanitized()
    }

    /// Sérialise les préférences en JSON lisible.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Charge les préférences depuis un fichier. Retourne `Default` en cas d'erreur,
    /// de fichier manquant, de taille > 64 Kio ou de corruption. Jamais de panique.
    pub fn load(path: &Path) -> Self {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > 64 * 1024 {
                return Self::default();
            }
        }
        std::fs::read_to_string(path)
            .ok()
            .map(|c| Self::from_json(&c))
            .unwrap_or_default()
    }

    /// Sauvegarde les préférences dans un fichier JSON de manière atomique.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        write_atomic(path, self.to_json().as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Crée un dossier temporaire unique contenant le pid et un compteur.
    fn make_temp_dir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("grx-overlay-prefs-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_defaults() {
        let p = OverlayPrefs::default();
        assert_eq!(p.version, 1);
        assert_eq!(p.menu_pos, None);
        assert!(!p.show_stats);
        assert!(p.button_enabled);
        assert_eq!(p.volume, 0.8);
        assert!(!p.muted);
        assert!(!p.vsync);
        assert!(!p.capture);
        assert_eq!(p.bitrate_cap_mbps, None);
        assert_eq!(p.fps_cap, None);
    }

    #[test]
    fn display_choice_is_optional_and_bounded() {
        // Ancien fichier sans le champ : None.
        assert_eq!(OverlayPrefs::from_json(r#"{"volume":0.5}"#).display, None);
        assert_eq!(OverlayPrefs::from_json(r#"{"display":""}"#).display, None);
        let long = format!(r#"{{"display":"{}"}}"#, "x".repeat(500));
        assert_eq!(OverlayPrefs::from_json(&long).display.unwrap().chars().count(), 128);
    }

    #[test]
    fn test_roundtrip_json() {
        let original = OverlayPrefs {
            version: 1,
            menu_pos: Some([0.2, 0.3]),
            stats_pos: Some([0.4, 0.5]),
            button_pos: Some([0.6, 0.7]),
            show_stats: true,
            button_enabled: false,
            volume: 0.5,
            muted: true,
            vsync: true,
            capture: true,
            bitrate_cap_mbps: Some(50),
            fps_cap: Some(60),
            display: Some("VG27A 2560x1440 @-2560,0".into()),
        };
        let json = original.to_json();
        let loaded = OverlayPrefs::from_json(&json);
        assert_eq!(original, loaded);
    }

    #[test]
    fn test_corrupt_json() {
        let loaded = OverlayPrefs::from_json("{ invalid json }");
        assert_eq!(loaded, OverlayPrefs::default());
    }

    #[test]
    fn test_missing_file() {
        let loaded = OverlayPrefs::load(Path::new("/nonexistent/path/overlay.json"));
        assert_eq!(loaded, OverlayPrefs::default());
    }

    #[test]
    fn test_partial_json() {
        let json = r#"{"volume":0.3}"#;
        let loaded = OverlayPrefs::from_json(json);
        assert_eq!(loaded.volume, 0.3);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.menu_pos, None);
        assert!(!loaded.show_stats);
    }

    #[test]
    fn test_out_of_bounds() {
        let json = r#"{
            "volume": 7.0,
            "fps_cap": 1000,
            "bitrate_cap_mbps": 0,
            "menu_pos": [5.0, -3.0]
        }"#;
        let loaded = OverlayPrefs::from_json(json);
        assert_eq!(loaded.volume, 1.0);
        assert_eq!(loaded.fps_cap, Some(240));
        assert_eq!(loaded.bitrate_cap_mbps, Some(1));
        assert_eq!(loaded.menu_pos, Some([1.0, 0.0]));
    }

    #[test]
    fn test_bom() {
        let json = "\u{FEFF}{\"volume\":0.5}";
        let loaded = OverlayPrefs::from_json(json);
        assert_eq!(loaded.volume, 0.5);
    }

    #[test]
    fn test_save_load() {
        let dir = make_temp_dir();
        let path = dir.join("overlay.json");
        let prefs = OverlayPrefs {
            volume: 0.7,
            show_stats: true,
            ..Default::default()
        };
        prefs.save(&path).unwrap();
        let loaded = OverlayPrefs::load(&path);
        assert_eq!(prefs, loaded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_file_too_large() {
        let dir = make_temp_dir();
        let path = dir.join("overlay.json");
        let large_data = vec![b'a'; 64 * 1024 + 1];
        fs::write(&path, &large_data).unwrap();
        let loaded = OverlayPrefs::load(&path);
        assert_eq!(loaded, OverlayPrefs::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
