//! Négociation du codec côté host (`GRX-COD` -> choix -> `GRX-CDC`), voir `grx_core::codec_neg`.
//!
//! * La boucle réseau passe l'annonce du client à [`CodecState::on_client`] ; le choix est fait
//!   par `codec_neg::choose` (préférence `run --codec`, encodeurs matériels du host).
//! * Le thread vidéo attend la décision au démarrage ([`CodecState::wait_decided`], au plus
//!   [`DECIDE_TIMEOUT`] : un ancien client n'annonce rien -> H.264 en `auto`), ouvre l'encodeur par
//!   [`open_encoder`] (repli H.264 si la création échoue) et, en cours de session, recrée
//!   l'encodeur quand le codec voulu change ([`CodecState::take_switch`]) : renégociation sans
//!   couper la session, le client reçoit une image clé du nouveau codec.
//! * La boucle réseau annonce le codec effectif à chaque changement ([`announce`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grx_core::codec_neg::{
    choose, BackendId, ChoiceReason, ClientCodecs, CodecCap, CodecPref, CodecSet, FormatSet, HostCodec, VideoCodec,
};
use grx_transport::Endpoint;
use grx_video::Codec;

/// Attente maximale de l'annonce du client avant de démarrer l'encodeur.
pub const DECIDE_TIMEOUT: Duration = Duration::from_millis(700);

pub fn to_neg(c: Codec) -> VideoCodec {
    match c {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
        Codec::Av1 => VideoCodec::Av1,
    }
}

pub fn from_neg(c: VideoCodec) -> Codec {
    match c {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::Hevc => Codec::Hevc,
        VideoCodec::Av1 => Codec::Av1,
    }
}

fn backend_of(name: &str) -> BackendId {
    let n = name.to_ascii_lowercase();
    if n.contains("nvidia") {
        BackendId::MF_NVIDIA
    } else if n.contains("amd") || n.contains("amf") {
        BackendId::MF_AMD
    } else if n.contains("intel") || n.contains("quick sync") {
        BackendId::MF_INTEL
    } else {
        BackendId::MF_OTHER
    }
}

/// Encodeurs matériels du host (énumération Media Foundation, en cache).
pub fn host_encoders() -> Vec<CodecCap> {
    grx_video::available_encoders()
        .into_iter()
        .map(|(c, n)| CodecCap { codec: to_neg(c), backend: backend_of(&n), formats: FormatSet::YUV420_8 })
        .collect()
}

struct St {
    pref: CodecPref,
    encoders: Vec<CodecCap>,
    client: Option<ClientCodecs>,
    decided: bool,
    wanted: VideoCodec,
    reason: ChoiceReason,
    /// Codecs dont l'encodeur n'a pas pu être créé pendant cette session.
    failed: CodecSet,
    active: Option<VideoCodec>,
    gen: u32,
    announced: u32,
}

impl St {
    fn usable(&self) -> CodecSet {
        let set: CodecSet = self.encoders.iter().map(|c| c.codec).collect();
        // H.264 est toujours tenté en dernier recours.
        set.with(VideoCodec::H264).intersect(CodecSet::from_bits(!self.failed.bits() & 7).unwrap_or_default())
    }
    fn recompute(&mut self) {
        let (c, r) = choose(self.pref, self.usable(), self.client.as_ref());
        self.wanted = c;
        self.reason = r;
    }
}

/// État de la négociation d'une session (partagé boucle réseau / thread vidéo).
pub struct CodecState {
    st: Mutex<St>,
    decided: AtomicBool,
}

impl CodecState {
    pub fn new(pref: CodecPref) -> CodecState {
        Self::with_encoders(pref, host_encoders())
    }

    pub fn with_encoders(pref: CodecPref, encoders: Vec<CodecCap>) -> CodecState {
        let mut st = St {
            pref,
            encoders,
            client: None,
            decided: false,
            wanted: VideoCodec::H264,
            reason: ChoiceReason::NoClientInfo,
            failed: CodecSet::EMPTY,
            active: None,
            gen: 0,
            announced: 0,
        };
        st.recompute();
        CodecState { st: Mutex::new(st), decided: AtomicBool::new(false) }
    }

    /// Annonce `GRX-COD` du client. Renvoie `false` si le message est invalide (ignoré).
    pub fn on_client(&self, msg: &[u8]) -> bool {
        let Some(c) = ClientCodecs::decode(msg) else { return false };
        let mut st = self.st.lock().unwrap();
        let caps: Vec<String> =
            c.decoders.iter().map(|d| format!("{} ({})", d.codec.label(), d.backend.label())).collect();
        st.client = Some(c.clone());
        st.recompute();
        st.decided = true;
        self.decided.store(true, Ordering::SeqCst);
        println!(
            "  client : décodeurs {}{}{} -> codec {} ({})",
            if caps.is_empty() { "aucun".to_string() } else { caps.join(", ") },
            c.preferred.map_or(String::new(), |p| format!(", demande {}", p.label())),
            if c.renegotiate { " [renégociation après panne du décodeur]" } else { "" },
            st.wanted.label(),
            st.reason.label()
        );
        true
    }

    /// Bloque (au plus `timeout`) jusqu'à l'annonce du client ; renvoie le codec voulu.
    pub fn wait_decided(&self, timeout: Duration, stop: &AtomicBool) -> Codec {
        let t0 = Instant::now();
        while !self.decided.load(Ordering::SeqCst) && t0.elapsed() < timeout && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut st = self.st.lock().unwrap();
        if !st.decided {
            st.decided = true;
            println!(
                "  client sans négociation de codec (ancienne version ?) : {} ({})",
                st.wanted.label(),
                st.reason.label()
            );
        }
        from_neg(st.wanted)
    }

    pub fn wanted(&self) -> Codec {
        from_neg(self.st.lock().unwrap().wanted)
    }

    /// Codec à adopter si différent de celui de l'encodeur courant.
    pub fn take_switch(&self, current: Codec) -> Option<Codec> {
        let st = self.st.lock().unwrap();
        (st.decided && st.wanted != to_neg(current)).then(|| from_neg(st.wanted))
    }

    /// L'encodeur de ce codec n'a pas pu être créé : repli (H.264 en dernier).
    pub fn encoder_failed(&self, c: Codec) {
        let mut st = self.st.lock().unwrap();
        if c != Codec::H264 {
            st.failed = st.failed.with(to_neg(c));
        }
        st.recompute();
        st.reason = ChoiceReason::Fallback;
    }

    /// L'encodeur de ce codec tourne : à annoncer au client.
    pub fn set_active(&self, c: Codec) {
        let mut st = self.st.lock().unwrap();
        if st.active != Some(to_neg(c)) {
            st.active = Some(to_neg(c));
            st.gen = st.gen.wrapping_add(1);
        }
    }

    /// Message `GRX-CDC` en attente d'envoi (codec effectif changé depuis la dernière annonce).
    fn pending(&self) -> Option<(u32, Vec<u8>)> {
        let st = self.st.lock().unwrap();
        let codec = st.active?;
        (st.gen != st.announced).then(|| {
            let reason = if codec == st.wanted { st.reason } else { ChoiceReason::Fallback };
            (st.gen, HostCodec { codec, format: FormatSet::YUV420_8, encoders: st.encoders.clone(), reason }.encode())
        })
    }

    fn mark_announced(&self, gen: u32) {
        self.st.lock().unwrap().announced = gen;
    }
}

/// Envoie `GRX-CDC` si le codec effectif a changé (appelé à chaque tour de la boucle réseau).
pub fn announce(ep: &mut Endpoint, cs: &CodecState) {
    if let Some((gen, msg)) = cs.pending() {
        if ep.send_control(&msg).is_ok() {
            cs.mark_announced(gen);
        }
    }
}

/// Ouvre un encodeur du codec voulu ; en cas d'échec, le marque indisponible et se replie
/// (jusqu'à H.264). `make` crée l'encodeur pour un codec donné.
pub fn open_encoder<E: std::fmt::Display>(
    cs: &CodecState,
    mut make: impl FnMut(Codec) -> Result<grx_video::Encoder, E>,
) -> Result<grx_video::Encoder, String> {
    loop {
        let c = cs.wanted();
        match make(c) {
            Ok(e) => {
                cs.set_active(c);
                return Ok(e);
            }
            Err(e) if c != Codec::H264 => {
                eprintln!("  encodeur {c:?} indisponible ({e}) : repli");
                cs.encoder_failed(c);
            }
            Err(e) => return Err(format!("encodeur : {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(set: &[VideoCodec]) -> Vec<CodecCap> {
        set.iter().map(|&c| CodecCap::new(c, BackendId::MF_NVIDIA)).collect()
    }

    fn client(set: &[VideoCodec], pref: Option<VideoCodec>, reneg: bool) -> Vec<u8> {
        let mut c = ClientCodecs::simple(set.iter().copied().collect(), BackendId::MEDIA_FOUNDATION, pref);
        c.renegotiate = reneg;
        c.encode()
    }

    const ALL: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];

    #[test]
    fn old_client_gets_h264_after_timeout() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        assert_eq!(cs.wait_decided(Duration::from_millis(30), &stop), Codec::H264);
        assert!(t0.elapsed() >= Duration::from_millis(25));
        // Un host forcé en HEVC garde son comportement historique avec un ancien client.
        let cs = CodecState::with_encoders(CodecPref::Force(VideoCodec::Hevc), caps(&ALL));
        assert_eq!(cs.wait_decided(Duration::from_millis(1), &stop), Codec::Hevc);
    }

    #[test]
    fn client_announcement_drives_choice_and_renegotiation() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        assert!(!cs.on_client(b"GRX-COD garbage"));
        assert!(cs.on_client(&client(&ALL, None, false)));
        let stop = AtomicBool::new(false);
        assert_eq!(cs.wait_decided(Duration::from_secs(5), &stop), Codec::Hevc);
        cs.set_active(Codec::Hevc);
        let (g, m) = cs.pending().expect("annonce à envoyer");
        assert_eq!(HostCodec::decode(&m).unwrap().codec, VideoCodec::Hevc);
        cs.mark_announced(g);
        assert!(cs.pending().is_none());
        assert_eq!(cs.take_switch(Codec::Hevc), None);
        // Le décodeur HEVC du client casse : il renégocie sans HEVC.
        assert!(cs.on_client(&client(&[VideoCodec::H264], None, true)));
        assert_eq!(cs.take_switch(Codec::Hevc), Some(Codec::H264));
        cs.set_active(Codec::H264);
        let (_, m) = cs.pending().unwrap();
        assert_eq!(HostCodec::decode(&m).unwrap().codec, VideoCodec::H264);
    }

    #[test]
    fn encoder_failure_falls_back_to_h264() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        cs.on_client(&client(&ALL, Some(VideoCodec::Av1), false));
        assert_eq!(cs.wanted(), Codec::Av1);
        cs.encoder_failed(Codec::Av1);
        assert_eq!(cs.wanted(), Codec::Hevc);
        cs.encoder_failed(Codec::Hevc);
        assert_eq!(cs.wanted(), Codec::H264);
        cs.encoder_failed(Codec::H264);
        assert_eq!(cs.wanted(), Codec::H264);
    }

    #[test]
    fn host_without_av1_encoder_never_picks_av1() {
        let cs = CodecState::with_encoders(CodecPref::Force(VideoCodec::Av1), caps(&[VideoCodec::H264, VideoCodec::Hevc]));
        cs.on_client(&client(&ALL, None, false));
        assert_eq!(cs.wanted(), Codec::Hevc);
    }

    #[test]
    fn backend_names() {
        assert_eq!(backend_of("NVIDIA HEVC Encoder MFT"), BackendId::MF_NVIDIA);
        assert_eq!(backend_of("AMDh265Encoder"), BackendId::MF_AMD);
        assert_eq!(backend_of("Intel® Quick Sync Video H.264 Encoder MFT"), BackendId::MF_INTEL);
        assert_eq!(backend_of("x"), BackendId::MF_OTHER);
    }
}
