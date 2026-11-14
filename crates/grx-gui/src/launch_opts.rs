//! Options de lancement de `grx-host run` et `grx-client play` (onglet Paramètres de grx-gui),
//! leur conversion en arguments et la sonde des décodeurs. Règle : une valeur par défaut ne
//! produit AUCUN argument (le défaut de l'exécutable s'applique).
//!
//! Ajouter une option : un champ (avec sa valeur par défaut dans `Default`), une ligne dans
//! `to_args`, un test ; puis le widget dans `app.rs` (`settings_tab`).
use std::path::Path;
use serde::{Deserialize, Serialize};
use crate::logic::DEFAULT_PORT;

// ---------------------------------------------------------------------------------------------
// Énumérations de choix
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum CodecChoice {
    #[default]
    Auto,
    Hevc,
    H264,
    Av1,
}

impl CodecChoice {
    pub const ALL: [CodecChoice; 4] = [CodecChoice::Auto, CodecChoice::Hevc, CodecChoice::H264, CodecChoice::Av1];

    pub fn arg(self) -> &'static str {
        match self {
            CodecChoice::Auto => "auto",
            CodecChoice::Hevc => "hevc",
            CodecChoice::H264 => "h264",
            CodecChoice::Av1 => "av1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CodecChoice::Auto => "Automatique (négocié)",
            CodecChoice::Hevc => "H.265 (HEVC)",
            CodecChoice::H264 => "H.264",
            CodecChoice::Av1 => "AV1 (RTX 40+)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum EncoderChoice {
    #[default]
    Auto,
    Nvidia,
    Amd,
    Intel,
    Software,
}

impl EncoderChoice {
    pub const ALL: [EncoderChoice; 5] = [EncoderChoice::Auto, EncoderChoice::Nvidia, EncoderChoice::Amd, EncoderChoice::Intel, EncoderChoice::Software];

    pub fn arg(self) -> &'static str {
        match self {
            EncoderChoice::Auto => "auto",
            EncoderChoice::Nvidia => "nvidia",
            EncoderChoice::Amd => "amd",
            EncoderChoice::Intel => "intel",
            EncoderChoice::Software => "software",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EncoderChoice::Auto => "Automatique (GPU de l'écran)",
            EncoderChoice::Nvidia => "NVIDIA (NVENC)",
            EncoderChoice::Amd => "AMD (AMF)",
            EncoderChoice::Intel => "Intel (Quick Sync)",
            EncoderChoice::Software => "Logiciel (OpenH264, H.264 seulement)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum ChromaChoice {
    #[default]
    Auto,
    C420,
    C444,
}

impl ChromaChoice {
    pub const ALL: [ChromaChoice; 3] = [ChromaChoice::Auto, ChromaChoice::C420, ChromaChoice::C444];

    pub fn arg(self) -> &'static str {
        match self {
            ChromaChoice::Auto => "auto",
            ChromaChoice::C420 => "420",
            ChromaChoice::C444 => "444",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChromaChoice::Auto => "Automatique (selon le client)",
            ChromaChoice::C420 => "4:2:0 (standard)",
            ChromaChoice::C444 => "4:4:4 (texte net, NVIDIA HEVC)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum HostGamepadType {
    #[default]
    Auto,
    Xbox,
    Ds4,
}

impl HostGamepadType {
    pub const ALL: [HostGamepadType; 3] = [HostGamepadType::Auto, HostGamepadType::Xbox, HostGamepadType::Ds4];

    pub fn arg(self) -> &'static str {
        match self {
            HostGamepadType::Auto => "auto",
            HostGamepadType::Xbox => "xbox",
            HostGamepadType::Ds4 => "ds4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HostGamepadType::Auto => "Choix du client",
            HostGamepadType::Xbox => "Toujours Xbox 360",
            HostGamepadType::Ds4 => "Toujours DualShock 4",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum ClientGamepadAs {
    #[default]
    Xbox,
    Auto,
    Ds4,
}

impl ClientGamepadAs {
    pub const ALL: [ClientGamepadAs; 3] = [ClientGamepadAs::Xbox, ClientGamepadAs::Auto, ClientGamepadAs::Ds4];

    pub fn arg(self) -> &'static str {
        match self {
            ClientGamepadAs::Xbox => "xbox",
            ClientGamepadAs::Auto => "auto",
            ClientGamepadAs::Ds4 => "ds4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ClientGamepadAs::Xbox => "Xbox 360 (toutes)",
            ClientGamepadAs::Auto => "Auto (DualShock 4 pour les manettes Sony)",
            ClientGamepadAs::Ds4 => "DualShock 4 (toutes)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum DecoderChoice {
    #[default]
    Remembered,
    Auto,
    Hardware,
    Software,
}

impl DecoderChoice {
    pub const ALL: [DecoderChoice; 4] = [DecoderChoice::Remembered, DecoderChoice::Auto, DecoderChoice::Hardware, DecoderChoice::Software];

    pub fn arg(self) -> Option<&'static str> {
        match self {
            DecoderChoice::Remembered => None,
            DecoderChoice::Auto => Some("auto"),
            DecoderChoice::Hardware => Some("hardware"),
            DecoderChoice::Software => Some("software"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DecoderChoice::Remembered => "Comme dans le menu du client",
            DecoderChoice::Auto => "Automatique (matériel, puis logiciel)",
            DecoderChoice::Hardware => "Matériel uniquement",
            DecoderChoice::Software => "Logiciel uniquement",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Options du host
// ---------------------------------------------------------------------------------------------

/// Options de `grx-host run` exposées dans l'interface.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRunOptions {
    /// Adresse d'écoute `ip:port`.
    pub bind: String,
    /// Codec vidéo principal.
    pub codec: CodecChoice,
    /// Encodeur vidéo prioritaire.
    pub encoder: EncoderChoice,
    /// Sous-échantillonnage chroma.
    pub chroma: ChromaChoice,
    /// Images par seconde.
    pub fps: u32,
    /// Débit vidéo maximal en Mb/s.
    pub max_bitrate: u32,
    /// Débit vidéo de départ en Mb/s.
    pub start_bitrate: u32,
    /// Remplit les lettres noires (letterboxing/pillarboxing).
    pub fill: bool,
    /// Désactive tout contrôle d'entrée (clavier, souris, manette).
    pub no_input: bool,
    /// Désactive le clavier (si entrée activée).
    pub no_keyboard: bool,
    /// Désactive la souris (si entrée activée).
    pub no_mouse: bool,
    /// Désactive la manette (si entrée activée).
    pub no_gamepad: bool,
    /// Type de manette forcé (si entrée activée).
    pub gamepad_type: HostGamepadType,
    /// Bloque la touche Windows (si entrée activée).
    pub block_win_key: bool,
    /// Désactive la diffusion audio.
    pub no_audio: bool,
    /// Active la découverte de port sur la box (UPnP/PCP/NAT-PMP).
    pub upnp: bool,
    /// Port externe souhaité sur la box (0 = même que le port d'écoute).
    pub external_port: u16,
    /// Délai d'inactivité avant déconnexion en secondes.
    pub idle_timeout: u32,
    /// Index de l'écran à capturer (0 = principal).
    pub display: u32,
}

impl Default for HostRunOptions {
    fn default() -> Self {
        Self {
            bind: format!("0.0.0.0:{DEFAULT_PORT}"),
            codec: CodecChoice::default(),
            encoder: EncoderChoice::default(),
            chroma: ChromaChoice::default(),
            fps: 60,
            max_bitrate: 50,
            start_bitrate: 20,
            fill: false,
            no_input: false,
            no_keyboard: false,
            no_mouse: false,
            no_gamepad: false,
            gamepad_type: HostGamepadType::default(),
            block_win_key: false,
            no_audio: false,
            upnp: true,
            external_port: 0,
            idle_timeout: 5,
            display: 0,
        }
    }
}

impl HostRunOptions {
    /// Valide les options et renvoie l'adresse d'écoute si elle est correcte.
    pub fn validate(&self) -> Result<std::net::SocketAddr, String> {
        let addr = self
            .bind
            .trim()
            .parse::<std::net::SocketAddr>()
            .map_err(|_| "Adresse d'écoute invalide : écrivez ip:port (ex. 0.0.0.0:7777).".to_string())?;
        if addr.port() == 0 {
            return Err("Le port d'écoute doit être entre 1 et 65535.".into());
        }
        if !(1..=240).contains(&self.fps) {
            return Err("La cadence doit être comprise entre 1 et 240 images/s.".into());
        }
        if !(1..=500).contains(&self.max_bitrate) {
            return Err("Le débit maximal doit être compris entre 1 et 500 Mb/s.".into());
        }
        if !(1..=self.max_bitrate).contains(&self.start_bitrate) {
            return Err("Le débit de départ doit être compris entre 1 Mb/s et le débit maximal.".into());
        }
        if !(1..=3600).contains(&self.idle_timeout) {
            return Err("Le délai d'inactivité doit être compris entre 1 et 3600 s.".into());
        }
        if self.display > 16 {
            return Err("Numéro d'écran invalide (0 = écran principal, 1 à 16).".into());
        }
        Ok(addr)
    }

    /// Convertit les options en arguments de ligne de commande pour `grx-host`.
    pub fn to_args(&self, data_dir: &Path) -> Vec<String> {
        let mut a = vec![
            "--data-dir".to_string(),
            data_dir.to_string_lossy().into_owned(),
            "run".into(),
        ];

        let default_bind = format!("0.0.0.0:{DEFAULT_PORT}");
        if self.bind.trim() != default_bind {
            a.push("--bind".into());
            a.push(self.bind.trim().to_string());
        }
        if self.codec != CodecChoice::Auto {
            a.push("--codec".into());
            a.push(self.codec.arg().into());
        }
        if self.encoder != EncoderChoice::Auto {
            a.push("--encoder".into());
            a.push(self.encoder.arg().into());
        }
        if self.chroma != ChromaChoice::Auto {
            a.push("--chroma".into());
            a.push(self.chroma.arg().into());
        }
        if self.fps != 60 {
            a.push("--fps".into());
            a.push(self.fps.to_string());
        }
        if self.max_bitrate != 50 {
            a.push("--max-bitrate".into());
            a.push(self.max_bitrate.to_string());
        }
        if self.start_bitrate != 20 {
            a.push("--start-bitrate".into());
            a.push(self.start_bitrate.to_string());
        }
        if self.fill {
            a.push("--fill".into());
        }
        if self.no_input {
            a.push("--no-input".into());
        } else {
            if self.no_keyboard { a.push("--no-keyboard".into()); }
            if self.no_mouse { a.push("--no-mouse".into()); }
            if self.no_gamepad { a.push("--no-gamepad".into()); }
            if !self.no_gamepad && self.gamepad_type != HostGamepadType::Auto {
                a.push("--gamepad-type".into());
                a.push(self.gamepad_type.arg().into());
            }
            if self.block_win_key { a.push("--block-win-key".into()); }
        }
        if self.no_audio {
            a.push("--no-audio".into());
        }
        if !self.upnp {
            a.push("--no-upnp".into());
        } else if self.external_port != 0 {
            a.push("--external-port".into());
            a.push(self.external_port.to_string());
        }
        if self.idle_timeout != 5 {
            a.push("--idle-timeout".into());
            a.push(self.idle_timeout.to_string());
        }
        if self.display != 0 {
            a.push("--display".into());
            a.push(self.display.to_string());
        }

        a
    }

    /// Retourne les réglages non par défaut à signaler par un badge dans l'interface.
    pub fn forced(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.codec != CodecChoice::Auto {
            v.push(format!("Codec {}", self.codec.label()));
        }
        if self.encoder != EncoderChoice::Auto {
            v.push(format!("Encodeur {}", self.encoder.label()));
        }
        if self.chroma != ChromaChoice::Auto {
            v.push(format!("Couleur {}", self.chroma.label()));
        }
        v
    }
}

// ---------------------------------------------------------------------------------------------
// Options du client
// ---------------------------------------------------------------------------------------------

/// Options de `grx-client play` exposées dans l'interface.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientPlayOptions {
    /// Désactive tout contrôle d'entrée.
    pub no_input: bool,
    /// Désactive la réception audio.
    pub no_audio: bool,
    /// Lance en plein écran.
    pub fullscreen: bool,
    /// Codec vidéo attendu.
    pub codec: CodecChoice,
    /// Stratégie de décodage.
    pub decoder: DecoderChoice,
    /// Volume de lecture (0-100, 80 = défaut).
    pub volume: u8,
    /// Active la synchronisation verticale.
    pub vsync: bool,
    /// Désactive la manette.
    pub no_gamepad: bool,
    /// Type de manette simulé.
    pub gamepad_as: ClientGamepadAs,
    /// Affiche le curseur local.
    pub local_cursor: bool,
    /// Affiche les statistiques de performance.
    pub stats: bool,
}

impl Default for ClientPlayOptions {
    fn default() -> Self {
        Self {
            no_input: false,
            no_audio: false,
            fullscreen: false,
            codec: CodecChoice::default(),
            decoder: DecoderChoice::default(),
            volume: 80,
            vsync: false,
            no_gamepad: false,
            gamepad_as: ClientGamepadAs::default(),
            local_cursor: false,
            stats: false,
        }
    }
}

impl ClientPlayOptions {
    /// Convertit les options en arguments de ligne de commande pour `grx-client`.
    /// `target` : nom d'un host mémorisé, ou `-` (lecture du lien depuis stdin).
    pub fn to_args(&self, data_dir: &Path, target: &str, save_as: Option<&str>) -> Vec<String> {
        let mut a = vec![
            "--data-dir".to_string(),
            data_dir.to_string_lossy().into_owned(),
            "play".into(),
            target.into(),
        ];
        if let Some(n) = save_as {
            a.push("--save-as".into());
            a.push(n.into());
        }
        if self.no_input { a.push("--no-input".into()); }
        if self.no_audio { a.push("--no-audio".into()); }
        if self.fullscreen { a.push("--fullscreen".into()); }
        if self.codec != CodecChoice::Auto {
            a.push("--codec".into());
            a.push(self.codec.arg().into());
        }
        if let Some(arg) = self.decoder.arg() {
            a.push("--decoder".into());
            a.push(arg.into());
        }
        if !self.no_audio && self.volume != 80 {
            a.push("--volume".into());
            a.push(format!("{:.2}", self.volume.min(100) as f32 / 100.0));
        }
        if self.vsync { a.push("--vsync".into()); }
        if self.no_gamepad { a.push("--no-gamepad".into()); }
        if !self.no_gamepad && self.gamepad_as != ClientGamepadAs::Xbox {
            a.push("--gamepad-as".into());
            a.push(self.gamepad_as.arg().into());
        }
        if self.local_cursor { a.push("--local-cursor".into()); }
        if self.stats { a.push("--stats".into()); }
        a
    }

    /// Retourne les réglages non par défaut à signaler par un badge dans l'interface.
    pub fn forced(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.codec != CodecChoice::Auto {
            v.push(format!("Codec {}", self.codec.label()));
        }
        if self.decoder != DecoderChoice::Remembered {
            v.push(format!("Décodeur {}", self.decoder.label()));
        }
        v
    }
}

// ---------------------------------------------------------------------------------------------
// Sonde des décodeurs
// ---------------------------------------------------------------------------------------------

/// Analyse la sortie JSON de `grx-client decoders` et retourne les codecs supportés.
pub fn parse_decodable(json: &str) -> Option<Vec<CodecChoice>> {
    let last_json_line = json
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty() && l.trim().starts_with('{'))?;
    let val: serde_json::Value = serde_json::from_str(last_json_line).ok()?;
    let arr = val.get("decodable")?.as_array()?;
    let mut res = Vec::new();
    for s in arr {
        if let Some(s) = s.as_str() {
            match s {
                "hevc" => res.push(CodecChoice::Hevc),
                "h264" => res.push(CodecChoice::H264),
                "av1" => res.push(CodecChoice::Av1),
                _ => {}
            }
        }
    }
    Some(res)
}

/// Retourne un message d'avertissement si le codec choisi n'est pas supporté par la machine cliente.
pub fn codec_warning(choice: CodecChoice, decodable: Option<&[CodecChoice]>) -> Option<String> {
    if choice == CodecChoice::Auto {
        return None;
    }
    let dec = decodable?;
    if dec.contains(&choice) {
        return None;
    }
    Some(format!(
        "Ce PC ne sait pas décoder {} : le host choisira un autre codec.",
        choice.label()
    ))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn defaults_produce_no_extra_args() {
        assert_eq!(
            HostRunOptions::default().to_args(Path::new("D")),
            vec!["--data-dir", "D", "run"]
        );
        assert_eq!(
            ClientPlayOptions::default().to_args(Path::new("D"), "salon", None),
            vec!["--data-dir", "D", "play", "salon"]
        );
    }

    #[test]
    fn host_bind() {
        let o = HostRunOptions { bind: " [::]:7777 ".into(), ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--bind", "[::]:7777"]));
    }

    #[test]
    fn host_codec() {
        let o = HostRunOptions { codec: CodecChoice::Hevc, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--codec", "hevc"]));
    }

    #[test]
    fn host_encoder() {
        let o = HostRunOptions { encoder: EncoderChoice::Nvidia, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--encoder", "nvidia"]));
    }

    #[test]
    fn host_chroma() {
        let o = HostRunOptions { chroma: ChromaChoice::C444, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--chroma", "444"]));
    }

    #[test]
    fn host_fps() {
        let o = HostRunOptions { fps: 30, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--fps", "30"]));
    }

    #[test]
    fn host_max_bitrate() {
        let o = HostRunOptions { max_bitrate: 100, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--max-bitrate", "100"]));
    }

    #[test]
    fn host_start_bitrate() {
        let o = HostRunOptions { start_bitrate: 10, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--start-bitrate", "10"]));
    }

    #[test]
    fn host_fill() {
        let o = HostRunOptions { fill: true, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.iter().any(|x| x == "--fill"));
    }

    #[test]
    fn host_no_input_suppresses_fine_filters() {
        let o = HostRunOptions { no_input: true, no_keyboard: true, block_win_key: true, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.iter().any(|x| x == "--no-input"));
        assert!(!a.iter().any(|x| x == "--no-keyboard" || x == "--block-win-key"));
    }

    #[test]
    fn host_gamepad_type() {
        let o = HostRunOptions { gamepad_type: HostGamepadType::Xbox, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--gamepad-type", "xbox"]));
    }

    #[test]
    fn host_no_gamepad_suppresses_gamepad_type() {
        let o = HostRunOptions { no_gamepad: true, gamepad_type: HostGamepadType::Ds4, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.iter().any(|x| x == "--no-gamepad"));
        assert!(!a.iter().any(|x| x == "--gamepad-type"));
    }

    #[test]
    fn host_block_win_key() {
        let o = HostRunOptions { block_win_key: true, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.iter().any(|x| x == "--block-win-key"));
    }

    #[test]
    fn host_no_audio() {
        let o = HostRunOptions { no_audio: true, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.iter().any(|x| x == "--no-audio"));
    }

    #[test]
    fn host_upnp_external_port() {
        let dir = Path::new("D");
        let a = HostRunOptions { external_port: 7790, ..Default::default() }.to_args(dir);
        assert!(a.windows(2).any(|w| w == ["--external-port", "7790"]));
        let a = HostRunOptions { upnp: false, external_port: 7790, ..Default::default() }.to_args(dir);
        assert!(a.iter().any(|x| x == "--no-upnp"));
        assert!(!a.iter().any(|x| x == "--external-port"));
    }

    #[test]
    fn host_idle_timeout() {
        let o = HostRunOptions { idle_timeout: 10, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--idle-timeout", "10"]));
    }

    #[test]
    fn host_display() {
        let o = HostRunOptions { display: 2, ..Default::default() };
        let a = o.to_args(Path::new("D"));
        assert!(a.windows(2).any(|w| w == ["--display", "2"]));
    }

    #[test]
    fn client_no_input() {
        let o = ClientPlayOptions { no_input: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--no-input"));
    }

    #[test]
    fn client_no_audio() {
        let o = ClientPlayOptions { no_audio: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--no-audio"));
    }

    #[test]
    fn client_fullscreen() {
        let o = ClientPlayOptions { fullscreen: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--fullscreen"));
    }

    #[test]
    fn client_codec() {
        let o = ClientPlayOptions { codec: CodecChoice::H264, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.windows(2).any(|w| w == ["--codec", "h264"]));
    }

    #[test]
    fn client_decoder() {
        let o = ClientPlayOptions { decoder: DecoderChoice::Hardware, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.windows(2).any(|w| w == ["--decoder", "hardware"]));
        let o = ClientPlayOptions { decoder: DecoderChoice::Remembered, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(!a.iter().any(|x| x == "--decoder"));
    }

    #[test]
    fn client_volume() {
        let o = ClientPlayOptions { volume: 50, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.windows(2).any(|w| w == ["--volume", "0.50"]));
        let o = ClientPlayOptions { volume: 100, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.windows(2).any(|w| w == ["--volume", "1.00"]));
        let o = ClientPlayOptions { volume: 80, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(!a.iter().any(|x| x == "--volume"));
        let o = ClientPlayOptions { no_audio: true, volume: 50, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(!a.iter().any(|x| x == "--volume"));
    }

    #[test]
    fn client_vsync() {
        let o = ClientPlayOptions { vsync: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--vsync"));
    }

    #[test]
    fn client_no_gamepad() {
        let o = ClientPlayOptions { no_gamepad: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--no-gamepad"));
    }

    #[test]
    fn client_gamepad_as() {
        let o = ClientPlayOptions { gamepad_as: ClientGamepadAs::Ds4, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.windows(2).any(|w| w == ["--gamepad-as", "ds4"]));
        let o = ClientPlayOptions { no_gamepad: true, gamepad_as: ClientGamepadAs::Ds4, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(!a.iter().any(|x| x == "--gamepad-as"));
    }

    #[test]
    fn client_local_cursor() {
        let o = ClientPlayOptions { local_cursor: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--local-cursor"));
    }

    #[test]
    fn client_stats() {
        let o = ClientPlayOptions { stats: true, ..Default::default() };
        let a = o.to_args(Path::new("D"), "salon", None);
        assert!(a.iter().any(|x| x == "--stats"));
    }

    #[test]
    fn client_args_keep_token_off_the_command_line() {
        let dir = Path::new("D");
        let o = ClientPlayOptions { no_input: true, no_audio: false, fullscreen: true, ..Default::default() };
        assert_eq!(
            o.to_args(dir, "-", Some("mon pc")),
            ["--data-dir", "D", "play", "-", "--save-as", "mon pc", "--no-input", "--fullscreen"]
        );
        assert_eq!(ClientPlayOptions::default().to_args(dir, "salon", None), ["--data-dir", "D", "play", "salon"]);
    }

    #[test]
    fn host_validation() {
        assert!(HostRunOptions::default().validate().is_ok());
        for (bind, fps) in [
            ("pas une adresse", 60),
            ("0.0.0.0:0", 60),
            ("0.0.0.0", 60),
            ("0.0.0.0:7777", 0),
            ("0.0.0.0:7777", 241),
        ] {
            let o = HostRunOptions { bind: bind.into(), fps, ..Default::default() };
            assert!(o.validate().is_err(), "{bind} {fps}");
        }
        let o = HostRunOptions { max_bitrate: 0, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { max_bitrate: 501, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { start_bitrate: 0, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { start_bitrate: 51, max_bitrate: 50, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { idle_timeout: 0, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { idle_timeout: 3601, ..Default::default() };
        assert!(o.validate().is_err());
        let o = HostRunOptions { display: 17, ..Default::default() };
        assert!(o.validate().is_err());
    }

    #[test]
    fn serde_retrocompat() {
        let o: HostRunOptions = serde_json::from_str(r#"{"bind":"0.0.0.0:7777","fps":30}"#).unwrap();
        assert_eq!(o.fps, 30);
        assert!(o.upnp);
        assert_eq!(o.max_bitrate, 50);

        let o: HostRunOptions = serde_json::from_str(r#"{"codec":"H264","champ_inconnu":1}"#).unwrap();
        assert_eq!(o.codec, CodecChoice::H264);

        let o: ClientPlayOptions = serde_json::from_str(r#"{"fullscreen":true}"#).unwrap();
        assert_eq!(o.volume, 80);

        let o = HostRunOptions { codec: CodecChoice::Hevc, encoder: EncoderChoice::Amd, ..Default::default() };
        let s = serde_json::to_string(&o).unwrap();
        let o2 = serde_json::from_str(&s).unwrap();
        assert_eq!(o, o2);

        let o = ClientPlayOptions { codec: CodecChoice::Av1, decoder: DecoderChoice::Software, volume: 60, ..Default::default() };
        let s = serde_json::to_string(&o).unwrap();
        let o2 = serde_json::from_str(&s).unwrap();
        assert_eq!(o, o2);
    }

    #[test]
    fn forced_defaults_and_non_defaults() {
        assert!(HostRunOptions::default().forced().is_empty());
        assert!(ClientPlayOptions::default().forced().is_empty());

        let o = HostRunOptions { codec: CodecChoice::H264, chroma: ChromaChoice::C444, ..Default::default() };
        assert_eq!(o.forced().len(), 2);

        let o = ClientPlayOptions { codec: CodecChoice::Hevc, decoder: DecoderChoice::Hardware, ..Default::default() };
        assert_eq!(o.forced().len(), 2);
    }

    #[test]
    fn parse_decodable_valid() {
        let json = "Some log line\n{\"decodable\":[\"hevc\",\"h264\",\"av1\"],\"details\":[\"a\",\"b\"]}";
        let res = parse_decodable(json).unwrap();
        assert_eq!(res, vec![CodecChoice::Hevc, CodecChoice::H264, CodecChoice::Av1]);
    }

    #[test]
    fn parse_decodable_ignores_unknown_and_invalid() {
        let json = "{\"decodable\":[\"hevc\",\"unknown\",\"h264\"]}";
        let res = parse_decodable(json).unwrap();
        assert_eq!(res, vec![CodecChoice::Hevc, CodecChoice::H264]);

        assert!(parse_decodable("not json").is_none());
        assert!(parse_decodable("{\"other\":true}").is_none());
    }

    #[test]
    fn codec_warning_logic() {
        assert!(codec_warning(CodecChoice::Auto, None).is_none());
        assert!(codec_warning(CodecChoice::Hevc, None).is_none());
        assert!(codec_warning(CodecChoice::Hevc, Some(&[CodecChoice::Hevc, CodecChoice::H264])).is_none());
        let w = codec_warning(CodecChoice::Av1, Some(&[CodecChoice::Hevc, CodecChoice::H264])).unwrap();
        assert!(w.contains("AV1"));
    }
}
