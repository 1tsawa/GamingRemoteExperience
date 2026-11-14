//! Négociation du codec vidéo entre le client et le host (canal Control fiable).
//!
//! * `GRX-COD` (client -> host) : [`ClientCodecs`], les décodeurs réellement utilisables par le
//!   client (sondés en créant un décodeur sur son GPU) et son éventuelle préférence (`play --codec`).
//!   Renvoyé en cours de session quand un décodeur casse (`renegotiate`) : le host change de
//!   codec sans couper la session et repart d'une image clé.
//! * `GRX-CDC` (host -> client) : [`HostCodec`], le codec choisi, les encodeurs du host et la
//!   raison du choix. Envoyé à chaque changement de codec.
//!
//! Format (petit-boutiste), extensible et strict :
//! `[magie 7 o][version u8 = 1][count u8][count x (clé u8, longueur u8, valeur)]`, au plus
//! [`MAX_ENTRIES`] entrées et [`MAX_LEN`] octets, longueur totale exacte. Une clé connue avec une
//! longueur incorrecte ou une valeur hors domaine fait refuser tout le message (`None`) ; une clé
//! inconnue est ignorée (champs futurs) ; une clé répétée : la dernière gagne. Dans une liste de
//! capacités ([`CodecCap`], 4 octets chacune), un codec ou un backend inconnu est ignoré : une
//! version future peut en ajouter sans casser les anciens pairs.
//!
//! Points d'extension prévus (phase 2 : AMF/QSV, logiciel, 4:4:4, 10 bits/HDR) :
//! * [`BackendId`] : identifiant ouvert (MF, FFmpeg D3D11VA, logiciel, NVENC, AMF, QSV…) ;
//! * [`FormatSet`] : chroma 4:2:0/4:4:4 x profondeur 8/10 bits par capacité, et format du flux
//!   annoncé par le host ([`HostCodec::format`]) ;
//! * [`DecoderMode`] : choix utilisateur Auto / matériel / logiciel ;
//! * octet réservé (profil) dans chaque capacité, clés 5..=255 libres.
//!
//! Rétrocompatibilité : un ancien host ignore `GRX-COD` (le client décode ce qu'il reçoit, le
//! codec est reconnu dans le flux) ; un ancien client n'envoie rien et le host en `auto` choisit
//! H.264, le choix le plus sûr (voir [`choose`]).

pub const CLIENT_CODECS_MAGIC: &[u8; 7] = b"GRX-COD";
pub const HOST_CODEC_MAGIC: &[u8; 7] = b"GRX-CDC";
pub const VERSION: u8 = 1;
pub const MAX_ENTRIES: usize = 16;
pub const MAX_LEN: usize = 512;
/// Nombre maximal de capacités par liste.
pub const MAX_CAPS: usize = 32;
const CAP_LEN: usize = 4;

/// Codec vidéo du protocole (indépendant de la plateforme ; `grx_video::Codec` côté Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];

    /// Identifiant sur le fil (1..=3 ; 0 = aucun).
    pub fn id(self) -> u8 {
        match self {
            VideoCodec::H264 => 1,
            VideoCodec::Hevc => 2,
            VideoCodec::Av1 => 3,
        }
    }

    pub fn from_id(id: u8) -> Option<VideoCodec> {
        match id {
            1 => Some(VideoCodec::H264),
            2 => Some(VideoCodec::Hevc),
            3 => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    fn bit(self) -> u8 {
        1 << (self.id() - 1)
    }

    /// Nom court affiché (overlay, journaux).
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// Valeur des options `--codec` (`h264`, `hevc`, `av1`).
    pub fn arg(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "av1",
        }
    }
}

/// Ensemble de codecs (bit 0 = H.264, bit 1 = HEVC, bit 2 = AV1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CodecSet(u8);

impl CodecSet {
    pub const EMPTY: CodecSet = CodecSet(0);
    const VALID: u8 = 0b111;

    pub fn from_bits(b: u8) -> Option<CodecSet> {
        (b & !Self::VALID == 0).then_some(CodecSet(b))
    }
    pub fn bits(self) -> u8 {
        self.0
    }
    pub fn with(self, c: VideoCodec) -> CodecSet {
        CodecSet(self.0 | c.bit())
    }
    pub fn without(self, c: VideoCodec) -> CodecSet {
        CodecSet(self.0 & !c.bit())
    }
    pub fn contains(self, c: VideoCodec) -> bool {
        self.0 & c.bit() != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn intersect(self, o: CodecSet) -> CodecSet {
        CodecSet(self.0 & o.0)
    }
    pub fn iter(self) -> impl Iterator<Item = VideoCodec> {
        VideoCodec::ALL.into_iter().filter(move |c| self.contains(*c))
    }
}

impl FromIterator<VideoCodec> for CodecSet {
    fn from_iter<I: IntoIterator<Item = VideoCodec>>(it: I) -> Self {
        it.into_iter().fold(CodecSet::EMPTY, CodecSet::with)
    }
}

impl std::fmt::Display for CodecSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("aucun");
        }
        let v: Vec<&str> = self.iter().map(VideoCodec::label).collect();
        f.write_str(&v.join(", "))
    }
}

/// Choix du codec d'un côté (`--codec auto|h264|hevc|av1`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecPref {
    #[default]
    Auto,
    Force(VideoCodec),
}

/// Backend de décodage/encodage (identifiant ouvert).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BackendId(pub u8);

impl BackendId {
    /// Décodeur Media Foundation matériel (DXVA/D3D11 ; extensions du Store pour HEVC/AV1).
    pub const MEDIA_FOUNDATION: BackendId = BackendId(1);
    /// libavcodec + hwaccel D3D11VA (DLL LGPL chargées dynamiquement).
    pub const FFMPEG_D3D11VA: BackendId = BackendId(2);
    /// Décodeur/encodeur logiciel (CPU) — réservé (phase 2).
    pub const SOFTWARE: BackendId = BackendId(3);
    /// Encodeur MFT NVIDIA (NVENC).
    pub const MF_NVIDIA: BackendId = BackendId(16);
    /// Encodeur MFT AMD (AMF) — réservé (phase 2).
    pub const MF_AMD: BackendId = BackendId(17);
    /// Encodeur MFT Intel (Quick Sync) — réservé (phase 2).
    pub const MF_INTEL: BackendId = BackendId(18);
    /// Autre MFT matériel.
    pub const MF_OTHER: BackendId = BackendId(19);

    /// Backends compris par cette version (les autres sont ignorés au décodage).
    pub fn known(self) -> bool {
        matches!(self.0, 1..=3 | 16..=19)
    }

    pub fn label(self) -> &'static str {
        match self {
            BackendId::MEDIA_FOUNDATION => "MF matériel",
            BackendId::FFMPEG_D3D11VA => "FFmpeg D3D11VA",
            BackendId::SOFTWARE => "logiciel",
            BackendId::MF_NVIDIA => "NVIDIA (MF)",
            BackendId::MF_AMD => "AMD (MF)",
            BackendId::MF_INTEL => "Intel (MF)",
            _ => "MF matériel",
        }
    }
}

/// Formats d'image : bit 0 = 4:2:0 8 bits, 1 = 4:2:0 10 bits, 2 = 4:4:4 8 bits, 3 = 4:4:4 10 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FormatSet(u8);

impl FormatSet {
    pub const YUV420_8: FormatSet = FormatSet(1);
    pub const YUV420_10: FormatSet = FormatSet(2);
    pub const YUV444_8: FormatSet = FormatSet(4);
    pub const YUV444_10: FormatSet = FormatSet(8);
    const VALID: u8 = 0x0F;

    pub fn from_bits(b: u8) -> Option<FormatSet> {
        (b & !Self::VALID == 0).then_some(FormatSet(b))
    }
    pub fn bits(self) -> u8 {
        self.0
    }
    /// `true` si tous les formats de `o` (non vide) sont présents.
    pub fn contains(self, o: FormatSet) -> bool {
        o.0 != 0 && self.0 & o.0 == o.0
    }
    pub fn union(self, o: FormatSet) -> FormatSet {
        FormatSet(self.0 | o.0)
    }
}

/// Une capacité : ce backend sait décoder (client) ou encoder (host) ce codec dans ces formats.
/// Sur le fil : `[codec u8][backend u8][formats u8][réservé u8 (profil), 0 aujourd'hui]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CodecCap {
    pub codec: VideoCodec,
    pub backend: BackendId,
    pub formats: FormatSet,
}

impl CodecCap {
    pub fn new(codec: VideoCodec, backend: BackendId) -> CodecCap {
        CodecCap { codec, backend, formats: FormatSet::YUV420_8 }
    }
}

/// Choix utilisateur du type de décodeur (réservé phase 2 ; aujourd'hui toujours `Auto`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecoderMode {
    #[default]
    Auto,
    Hardware,
    Software,
}

/// Annonce du client (`GRX-COD`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientCodecs {
    /// Décodeurs réellement utilisables (créés avec succès sur le GPU du client), préférés d'abord.
    pub decoders: Vec<CodecCap>,
    /// Codec demandé par l'utilisateur du client (`play --codec`), `None` = auto.
    pub preferred: Option<VideoCodec>,
    /// Envoyé après une panne de décodeur en cours de session (journal côté host).
    pub renegotiate: bool,
    pub decoder_mode: DecoderMode,
}

impl ClientCodecs {
    /// Codecs décodables en 4:2:0 8 bits (le format diffusé aujourd'hui).
    pub fn decodable(&self) -> CodecSet {
        self.decoders.iter().filter(|c| c.formats.contains(FormatSet::YUV420_8)).map(|c| c.codec).collect()
    }

    /// Annonce simple : un décodeur `backend` par codec de `set`.
    pub fn simple(set: CodecSet, backend: BackendId, preferred: Option<VideoCodec>) -> ClientCodecs {
        ClientCodecs { decoders: set.iter().map(|c| CodecCap::new(c, backend)).collect(), preferred, ..Default::default() }
    }

    /// Backend annoncé pour ce codec (le premier).
    pub fn backend_for(&self, c: VideoCodec) -> Option<BackendId> {
        self.decoders.iter().find(|d| d.codec == c).map(|d| d.backend)
    }
}

/// Pourquoi le host a choisi ce codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceReason {
    /// Préférence automatique du host parmi les codecs communs.
    Auto,
    /// Demandé par le client (`play --codec`).
    ClientPreference,
    /// Imposé par le host (`run --codec`).
    HostForced,
    /// Le client n'a rien annoncé (ancien client) : H.264 en `auto`, sinon le codec du host.
    NoClientInfo,
    /// Le codec demandé n'est pas utilisable des deux côtés : repli.
    Fallback,
}

impl ChoiceReason {
    fn id(self) -> u8 {
        match self {
            ChoiceReason::Auto => 0,
            ChoiceReason::ClientPreference => 1,
            ChoiceReason::HostForced => 2,
            ChoiceReason::NoClientInfo => 3,
            ChoiceReason::Fallback => 4,
        }
    }
    fn from_id(v: u8) -> Option<ChoiceReason> {
        Some(match v {
            0 => ChoiceReason::Auto,
            1 => ChoiceReason::ClientPreference,
            2 => ChoiceReason::HostForced,
            3 => ChoiceReason::NoClientInfo,
            4 => ChoiceReason::Fallback,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            ChoiceReason::Auto => "automatique",
            ChoiceReason::ClientPreference => "demandé par le client",
            ChoiceReason::HostForced => "imposé par le host",
            ChoiceReason::NoClientInfo => "client sans négociation",
            ChoiceReason::Fallback => "repli",
        }
    }
}

/// Annonce du host (`GRX-CDC`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCodec {
    pub codec: VideoCodec,
    /// Format du flux (un seul bit ; 4:2:0 8 bits aujourd'hui).
    pub format: FormatSet,
    /// Encodeurs matériels disponibles sur le host.
    pub encoders: Vec<CodecCap>,
    pub reason: ChoiceReason,
}

impl HostCodec {
    pub fn encoder_set(&self) -> CodecSet {
        self.encoders.iter().map(|c| c.codec).collect()
    }
}

// Clés communes aux deux messages.
const K_CAPS: u8 = 1; // client : décodeurs ; host : encodeurs
const K_CODEC: u8 = 2; // client : préférence (0 = auto) ; host : codec choisi (obligatoire)
const K_FLAGS: u8 = 3; // client : bit 0 = renégociation ; host : raison du choix
const K_MODE: u8 = 4; // client : DecoderMode ; host : format du flux (un bit de FormatSet)

fn encode_entries(magic: &[u8; 7], entries: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + entries.iter().map(|e| 2 + e.1.len()).sum::<usize>());
    v.extend_from_slice(magic);
    v.push(VERSION);
    v.push(entries.len() as u8);
    for (k, val) in entries {
        debug_assert!(val.len() <= 255);
        v.push(*k);
        v.push(val.len() as u8);
        v.extend_from_slice(val);
    }
    v
}

fn decode_entries<'a>(magic: &[u8; 7], b: &'a [u8]) -> Option<Vec<(u8, &'a [u8])>> {
    if b.len() < 9 || b.len() > MAX_LEN || &b[..7] != magic || b[7] != VERSION {
        return None;
    }
    let count = b[8] as usize;
    if count > MAX_ENTRIES {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    let mut i = 9usize;
    for _ in 0..count {
        let k = *b.get(i)?;
        let len = *b.get(i + 1)? as usize;
        let val = b.get(i + 2..i + 2 + len)?;
        out.push((k, val));
        i += 2 + len;
    }
    (i == b.len()).then_some(out)
}

fn encode_caps(caps: &[CodecCap]) -> Vec<u8> {
    caps.iter().take(MAX_CAPS).flat_map(|c| [c.codec.id(), c.backend.0, c.formats.bits(), 0]).collect()
}

/// Liste de capacités : longueur multiple de 4 et bornée ; entrées inconnues ignorées.
fn decode_caps(v: &[u8]) -> Option<Vec<CodecCap>> {
    if !v.len().is_multiple_of(CAP_LEN) || v.len() / CAP_LEN > MAX_CAPS {
        return None;
    }
    Some(
        v.as_chunks::<CAP_LEN>()
            .0
            .iter()
            .filter_map(|r| {
                let codec = VideoCodec::from_id(r[0])?;
                let backend = BackendId(r[1]);
                // Formats futurs (bits inconnus) : on ne garde que ceux qu'on comprend.
                let formats = FormatSet(r[2] & FormatSet::VALID);
                (backend.known() && formats.0 != 0).then_some(CodecCap { codec, backend, formats })
            })
            .collect(),
    )
}

fn one(v: &[u8]) -> Option<u8> {
    (v.len() == 1).then(|| v[0])
}

pub fn is_client_codecs(b: &[u8]) -> bool {
    b.starts_with(CLIENT_CODECS_MAGIC)
}

pub fn is_host_codec(b: &[u8]) -> bool {
    b.starts_with(HOST_CODEC_MAGIC)
}

impl ClientCodecs {
    pub fn encode(&self) -> Vec<u8> {
        let mode = match self.decoder_mode {
            DecoderMode::Auto => 0,
            DecoderMode::Hardware => 1,
            DecoderMode::Software => 2,
        };
        encode_entries(
            CLIENT_CODECS_MAGIC,
            &[
                (K_CAPS, encode_caps(&self.decoders)),
                (K_CODEC, vec![self.preferred.map_or(0, VideoCodec::id)]),
                (K_FLAGS, vec![self.renegotiate as u8]),
                (K_MODE, vec![mode]),
            ],
        )
    }

    pub fn decode(b: &[u8]) -> Option<ClientCodecs> {
        let mut c = ClientCodecs::default();
        for (k, v) in decode_entries(CLIENT_CODECS_MAGIC, b)? {
            match k {
                K_CAPS => c.decoders = decode_caps(v)?,
                K_CODEC => {
                    c.preferred = match one(v)? {
                        0 => None,
                        p => Some(VideoCodec::from_id(p)?),
                    }
                }
                K_FLAGS => {
                    let f = one(v)?;
                    if f > 1 {
                        return None;
                    }
                    c.renegotiate = f == 1;
                }
                K_MODE => {
                    c.decoder_mode = match one(v)? {
                        0 => DecoderMode::Auto,
                        1 => DecoderMode::Hardware,
                        2 => DecoderMode::Software,
                        _ => return None,
                    }
                }
                _ => {}
            }
        }
        Some(c)
    }
}

impl HostCodec {
    pub fn encode(&self) -> Vec<u8> {
        encode_entries(
            HOST_CODEC_MAGIC,
            &[
                (K_CAPS, encode_caps(&self.encoders)),
                (K_CODEC, vec![self.codec.id()]),
                (K_FLAGS, vec![self.reason.id()]),
                (K_MODE, vec![self.format.bits()]),
            ],
        )
    }

    pub fn decode(b: &[u8]) -> Option<HostCodec> {
        let (mut codec, mut reason, mut format, mut encoders) = (None, ChoiceReason::Auto, FormatSet::YUV420_8, Vec::new());
        for (k, v) in decode_entries(HOST_CODEC_MAGIC, b)? {
            match k {
                K_CAPS => encoders = decode_caps(v)?,
                K_CODEC => codec = Some(VideoCodec::from_id(one(v)?)?),
                K_FLAGS => reason = ChoiceReason::from_id(one(v)?)?,
                K_MODE => {
                    format = FormatSet::from_bits(one(v)?)?;
                    if format.bits().count_ones() != 1 {
                        return None;
                    }
                }
                _ => {}
            }
        }
        // Le codec est obligatoire.
        Some(HostCodec { codec: codec?, format, encoders, reason })
    }
}

/// Ordre de préférence automatique : HEVC (meilleure qualité à débit égal, décodage très
/// répandu) puis H.264 (universel). AV1 n'est jamais choisi automatiquement : seulement sur
/// demande (`--codec av1` d'un côté ou de l'autre) et s'il est encodable ET décodable.
pub const AUTO_ORDER: [VideoCodec; 2] = [VideoCodec::Hevc, VideoCodec::H264];

/// Choix du codec par le host.
///
/// * `host_pref` : `run --codec` ; `host_enc` : encodeurs matériels du host ;
/// * `client` : annonce `GRX-COD`, `None` pour un ancien client.
///
/// Règles : sans annonce, `auto` donne H.264 (choix sûr) et un codec forcé est respecté s'il
/// est encodable (comportement historique). Avec une annonce, seuls les codecs communs sont
/// candidats ; la préférence du client l'emporte, puis celle du host, puis [`AUTO_ORDER`].
/// S'il n'y a aucun codec commun, H.264 (le flux le plus probablement décodable).
pub fn choose(host_pref: CodecPref, host_enc: CodecSet, client: Option<&ClientCodecs>) -> (VideoCodec, ChoiceReason) {
    let auto_pick = |cands: CodecSet| AUTO_ORDER.into_iter().find(|c| cands.contains(*c));
    let Some(cl) = client else {
        return match host_pref {
            CodecPref::Force(c) if host_enc.contains(c) => (c, ChoiceReason::HostForced),
            CodecPref::Force(_) => (VideoCodec::H264, ChoiceReason::Fallback),
            CodecPref::Auto => (VideoCodec::H264, ChoiceReason::NoClientInfo),
        };
    };
    let cands = host_enc.intersect(cl.decodable());
    if let Some(p) = cl.preferred.filter(|p| cands.contains(*p)) {
        return (p, ChoiceReason::ClientPreference);
    }
    let wanted_but_missing = cl.preferred.is_some();
    match host_pref {
        CodecPref::Force(c) if cands.contains(c) => {
            return (c, if wanted_but_missing { ChoiceReason::Fallback } else { ChoiceReason::HostForced })
        }
        CodecPref::Force(_) => {
            return (auto_pick(cands).unwrap_or(VideoCodec::H264), ChoiceReason::Fallback);
        }
        CodecPref::Auto => {}
    }
    match auto_pick(cands) {
        Some(c) if !wanted_but_missing => (c, ChoiceReason::Auto),
        Some(c) => (c, ChoiceReason::Fallback),
        // Seul AV1 en commun (sans demande explicite) : on le prend plutôt qu'un flux indécodable.
        None if cands.contains(VideoCodec::Av1) => (VideoCodec::Av1, ChoiceReason::Fallback),
        None => (VideoCodec::H264, ChoiceReason::Fallback),
    }
}

#[cfg(test)]
#[path = "codec_neg_tests.rs"]
mod tests;
