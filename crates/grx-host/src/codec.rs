//! Négociation du codec côté host (`GRX-COD` -> choix -> `GRX-CDC`), voir `grx_core::codec_neg`.
//!
//! * La boucle réseau passe l'annonce du client à [`CodecState::on_client`] ; le choix est fait
//!   par `codec_neg::choose` (préférence `run --codec`, encodeurs du host) puis le format par
//!   `codec_neg::choose_format` (`run --chroma`, préférences 4:4:4 / 10 bits du client, source HDR).
//! * Le thread vidéo attend la décision au démarrage ([`CodecState::wait_decided`], au plus
//!   [`DECIDE_TIMEOUT`] : un ancien client n'annonce rien -> H.264 en `auto`), ouvre l'encodeur par
//!   [`open_encoder`] (repli : format de base 4:2:0 8 bits, puis H.264, si la création échoue) et,
//!   en cours de session, recrée l'encodeur quand le codec ou le format voulu change
//!   ([`CodecState::take_switch`]) : renégociation sans couper la session.
//! * La boucle réseau annonce le codec, le format, l'encodeur et le transfert effectifs à chaque
//!   changement ([`announce`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grx_core::codec_neg::{
    choose, choose_format, choose_transfer, BackendId, ChoiceReason, ChromaPref, ClientCodecs, CodecCap, CodecPref, CodecSet,
    FormatSet, HostCodec, Transfer, VideoCodec,
};
use grx_transport::Endpoint;
use grx_video::{Codec, EncoderKind, EncoderPref, PixelFormat, Vendor};

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

pub fn fmt_to_neg(f: PixelFormat) -> FormatSet {
    FormatSet::single(f.is_444(), f.is_10bit())
}

pub fn fmt_from_neg(f: FormatSet) -> PixelFormat {
    PixelFormat::new(f.is_444(), f.is_10bit())
}

/// Identifiant de protocole d'une famille d'encodeur.
pub fn backend_id(k: EncoderKind) -> BackendId {
    match k {
        EncoderKind::Mf(Vendor::Nvidia) => BackendId::MF_NVIDIA,
        EncoderKind::Mf(Vendor::Amd) => BackendId::MF_AMD,
        EncoderKind::Mf(Vendor::Intel) => BackendId::MF_INTEL,
        EncoderKind::Mf(_) => BackendId::MF_OTHER,
        EncoderKind::Nvenc => BackendId::NVENC,
        EncoderKind::OpenH264 => BackendId::SOFTWARE,
    }
}

/// L'encodeur `k` est-il autorisé par `run --encoder` ?
pub fn allowed(pref: EncoderPref, k: EncoderKind) -> bool {
    match (pref, k) {
        (EncoderPref::Auto, _) => true,
        (EncoderPref::Software, EncoderKind::OpenH264) => true,
        (EncoderPref::Software, _) => false,
        (_, EncoderKind::OpenH264) => false,
        (EncoderPref::Nvidia, EncoderKind::Nvenc) => true,
        (p, EncoderKind::Mf(v)) => p.vendor() == Some(v),
        _ => false,
    }
}

/// Capacités annoncées depuis une liste d'encodeurs (filtrée par `run --encoder`).
pub fn caps_from(pref: EncoderPref, list: &[grx_video::EncoderInfo]) -> Vec<CodecCap> {
    list.iter()
        .filter(|e| allowed(pref, e.kind))
        .map(|e| CodecCap {
            codec: to_neg(e.codec),
            backend: backend_id(e.kind),
            formats: e.formats.iter().fold(FormatSet::default(), |a, f| a.union(fmt_to_neg(*f))),
        })
        .collect()
}

/// Encodeurs du host sur l'adaptateur `adapter` (`None` : adaptateur préféré), en cache.
pub fn host_encoders(pref: EncoderPref, adapter: Option<u32>) -> Vec<CodecCap> {
    caps_from(pref, &grx_video::encoders_for_adapter(adapter))
}

/// Ce que le thread vidéo doit encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Want {
    pub codec: Codec,
    pub format: PixelFormat,
    /// Flux HDR10 (source HDR + 10 bits).
    pub hdr: bool,
}

struct St {
    pref: CodecPref,
    chroma: ChromaPref,
    hdr_source: bool,
    encoders: Vec<CodecCap>,
    client: Option<ClientCodecs>,
    decided: bool,
    wanted: VideoCodec,
    format: FormatSet,
    reason: ChoiceReason,
    failed: CodecSet,
    /// Formats dont l'encodeur n'a pas pu être créé (repli vers 4:2:0 8 bits).
    failed_formats: FormatSet,
    active: Option<(VideoCodec, FormatSet, Option<BackendId>)>,
    gen: u32,
    announced: u32,
}

impl St {
    fn usable(&self) -> CodecSet {
        let set: CodecSet = self.encoders.iter().map(|c| c.codec).collect();
        set.with(VideoCodec::H264).intersect(CodecSet::from_bits(!self.failed.bits() & 7).unwrap_or_default())
    }
    fn recompute(&mut self) {
        let (c, r) = choose(self.pref, self.usable(), self.client.as_ref());
        self.wanted = c;
        self.reason = r;
        let caps: Vec<CodecCap> = self
            .encoders
            .iter()
            .map(|e| CodecCap { formats: FormatSet::from_bits(e.formats.bits() & !self.failed_formats.bits()).unwrap_or_default(), ..*e })
            .collect();
        self.format = choose_format(c, &caps, self.client.as_ref(), self.chroma, self.hdr_source);
    }
    fn want(&self) -> Want {
        let transfer = choose_transfer(self.format, self.hdr_source);
        Want { codec: from_neg(self.wanted), format: fmt_from_neg(self.format), hdr: transfer == Transfer::Pq }
    }
}

/// État de la négociation d'une session (partagé boucle réseau / thread vidéo).
pub struct CodecState {
    st: Mutex<St>,
    decided: AtomicBool,
}

impl CodecState {
    pub fn new(pref: CodecPref, enc: EncoderPref, chroma: ChromaPref, adapter: Option<u32>, hdr_source: bool) -> CodecState {
        let s = Self::with_encoders(pref, host_encoders(enc, adapter));
        {
            let mut st = s.st.lock().unwrap();
            st.chroma = chroma;
            st.hdr_source = hdr_source;
            st.recompute();
        }
        s
    }

    pub fn with_encoders(pref: CodecPref, encoders: Vec<CodecCap>) -> CodecState {
        let mut st = St {
            pref,
            chroma: ChromaPref::Auto,
            hdr_source: false,
            encoders,
            client: None,
            decided: false,
            wanted: VideoCodec::H264,
            format: FormatSet::YUV420_8,
            reason: ChoiceReason::NoClientInfo,
            failed: CodecSet::EMPTY,
            failed_formats: FormatSet::default(),
            active: None,
            gen: 0,
            announced: 0,
        };
        st.recompute();
        CodecState { st: Mutex::new(st), decided: AtomicBool::new(false) }
    }

    /// Réglages (tests) : `run --chroma` et source HDR.
    #[cfg(test)]
    pub fn configure(&self, chroma: ChromaPref, hdr_source: bool) {
        let mut st = self.st.lock().unwrap();
        st.chroma = chroma;
        st.hdr_source = hdr_source;
        st.recompute();
    }

    /// Annonce `GRX-COD` reçue : met à jour le choix. Renvoie `false` si le message est invalide.
    pub fn on_client(&self, msg: &[u8]) -> bool {
        let Some(c) = ClientCodecs::decode(msg) else { return false };
        let mut st = self.st.lock().unwrap();
        let caps: Vec<String> = c
            .decoders
            .iter()
            .map(|d| format!("{} ({}, {})", d.codec.label(), d.backend.label(), d.formats.label()))
            .collect();
        st.client = Some(c.clone());
        st.recompute();
        st.decided = true;
        self.decided.store(true, Ordering::SeqCst);
        println!(
            "  client : décodeurs {}{}{}{} -> codec {} {} ({})",
            if caps.is_empty() { "aucun".to_string() } else { caps.join(", ") },
            c.preferred.map_or(String::new(), |p| format!(", demande {}", p.label())),
            if c.prefs.yuv444 || c.prefs.ten_bit {
                format!(", préfère{}{}", if c.prefs.yuv444 { " 4:4:4" } else { "" }, if c.prefs.ten_bit { " 10 bits" } else { "" })
            } else {
                String::new()
            },
            if c.renegotiate { " [renégociation]" } else { "" },
            st.wanted.label(),
            st.format.label(),
            st.reason.label()
        );
        true
    }

    /// Attend l'annonce du client (au plus `timeout`) puis renvoie ce qu'il faut encoder.
    pub fn wait_decided(&self, timeout: Duration, stop: &AtomicBool) -> Want {
        let t0 = Instant::now();
        while !self.decided.load(Ordering::SeqCst) && t0.elapsed() < timeout && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut st = self.st.lock().unwrap();
        if !st.decided {
            st.decided = true;
            println!("  client sans négociation de codec (ancienne version ?) : {} ({})", st.wanted.label(), st.reason.label());
        }
        st.want()
    }

    pub fn wanted(&self) -> Want {
        self.st.lock().unwrap().want()
    }

    /// Nouveau codec/format voulu, différent de celui de l'encodeur courant (après une annonce).
    pub fn take_switch(&self, current: Want) -> Option<Want> {
        let st = self.st.lock().unwrap();
        let w = st.want();
        (st.decided && w != current).then_some(w)
    }

    /// La création de l'encodeur `w` a échoué : un format évolué retombe en 4:2:0 8 bits, un codec
    /// en 4:2:0 8 bits est retiré (H.264 jamais : dernier recours).
    pub fn encoder_failed(&self, w: Want) {
        let mut st = self.st.lock().unwrap();
        if w.format != PixelFormat::Yuv420 {
            let f = st.failed_formats.union(fmt_to_neg(w.format));
            st.failed_formats = f;
        } else if w.codec != Codec::H264 {
            st.failed = st.failed.with(to_neg(w.codec));
        }
        st.recompute();
        st.reason = ChoiceReason::Fallback;
    }

    /// L'encodeur `w` (backend `b`) tourne : à annoncer au client.
    pub fn set_active(&self, w: Want, b: Option<BackendId>) {
        let mut st = self.st.lock().unwrap();
        let a = Some((to_neg(w.codec), fmt_to_neg(w.format), b));
        if st.active != a {
            st.active = a;
            st.gen = st.gen.wrapping_add(1);
        }
    }

    fn pending(&self) -> Option<(u32, Vec<u8>)> {
        let st = self.st.lock().unwrap();
        let (codec, format, encoder) = st.active?;
        (st.gen != st.announced).then(|| {
            let reason = if codec == st.wanted && format == st.format { st.reason } else { ChoiceReason::Fallback };
            let transfer = choose_transfer(format, st.hdr_source);
            (st.gen, HostCodec { codec, format, encoders: st.encoders.clone(), reason, encoder, transfer }.encode())
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

/// Ouvre un encodeur pour ce qui est voulu ; en cas d'échec, le marque indisponible et se replie
/// (format de base, puis H.264). `make` crée l'encodeur et renvoie son backend.
pub fn open_encoder<T, E: std::fmt::Display>(
    cs: &CodecState,
    mut make: impl FnMut(Want) -> Result<(T, Option<BackendId>), E>,
) -> Result<(T, Want), String> {
    loop {
        let w = cs.wanted();
        match make(w) {
            Ok((e, b)) => {
                cs.set_active(w, b);
                return Ok((e, w));
            }
            Err(e) if w.codec != Codec::H264 || w.format != PixelFormat::Yuv420 => {
                eprintln!("  encodeur {:?} {} indisponible ({e}) : repli", w.codec, w.format.label());
                cs.encoder_failed(w);
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

    fn want(codec: Codec) -> Want {
        Want { codec, format: PixelFormat::Yuv420, hdr: false }
    }

    #[test]
    fn old_client_gets_h264_after_timeout() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        assert_eq!(cs.wait_decided(Duration::from_millis(30), &stop).codec, Codec::H264);
        assert!(t0.elapsed() >= Duration::from_millis(25));
        let cs = CodecState::with_encoders(CodecPref::Force(VideoCodec::Hevc), caps(&ALL));
        assert_eq!(cs.wait_decided(Duration::from_millis(1), &stop).codec, Codec::Hevc);
    }

    #[test]
    fn client_announcement_drives_choice_and_renegotiation() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        assert!(!cs.on_client(b"GRX-COD garbage"));
        assert!(cs.on_client(&client(&ALL, None, false)));
        let stop = AtomicBool::new(false);
        assert_eq!(cs.wait_decided(Duration::from_secs(5), &stop).codec, Codec::Hevc);
        cs.set_active(want(Codec::Hevc), Some(BackendId::MF_NVIDIA));
        let (g, m) = cs.pending().expect("annonce à envoyer");
        let h = HostCodec::decode(&m).unwrap();
        assert_eq!((h.codec, h.encoder), (VideoCodec::Hevc, Some(BackendId::MF_NVIDIA)));
        cs.mark_announced(g);
        assert!(cs.pending().is_none());
        assert_eq!(cs.take_switch(want(Codec::Hevc)), None);
        assert!(cs.on_client(&client(&[VideoCodec::H264], None, true)));
        assert_eq!(cs.take_switch(want(Codec::Hevc)), Some(want(Codec::H264)));
        cs.set_active(want(Codec::H264), None);
        let (_, m) = cs.pending().unwrap();
        assert_eq!(HostCodec::decode(&m).unwrap().codec, VideoCodec::H264);
    }

    #[test]
    fn encoder_failure_falls_back_to_h264() {
        let cs = CodecState::with_encoders(CodecPref::Auto, caps(&ALL));
        cs.on_client(&client(&ALL, Some(VideoCodec::Av1), false));
        assert_eq!(cs.wanted().codec, Codec::Av1);
        cs.encoder_failed(want(Codec::Av1));
        assert_eq!(cs.wanted().codec, Codec::Hevc);
        cs.encoder_failed(want(Codec::Hevc));
        assert_eq!(cs.wanted().codec, Codec::H264);
        cs.encoder_failed(want(Codec::H264));
        assert_eq!(cs.wanted().codec, Codec::H264);
    }

    #[test]
    fn host_without_av1_encoder_never_picks_av1() {
        let cs = CodecState::with_encoders(CodecPref::Force(VideoCodec::Av1), caps(&[VideoCodec::H264, VideoCodec::Hevc]));
        cs.on_client(&client(&ALL, None, false));
        assert_eq!(cs.wanted().codec, Codec::Hevc);
    }

    #[test]
    fn format_negotiation_444_10bit_and_fallback() {
        // Host : HEVC MF 4:2:0 8/10 bits + NVENC 4:4:4 ; client : HEVC 4:2:0 8/10 + 4:4:4.
        let mut encs = caps(&ALL);
        encs[1].formats = FormatSet::YUV420_8 | FormatSet::YUV420_10;
        encs.push(CodecCap { codec: VideoCodec::Hevc, backend: BackendId::NVENC, formats: FormatSet::YUV444_8 });
        let cs = CodecState::with_encoders(CodecPref::Auto, encs);
        let mut c = ClientCodecs::simple(CodecSet::EMPTY.with(VideoCodec::Hevc).with(VideoCodec::H264), BackendId::MEDIA_FOUNDATION, None);
        c.decoders.iter_mut().find(|d| d.codec == VideoCodec::Hevc).unwrap().formats = FormatSet::YUV420_8 | FormatSet::YUV420_10 | FormatSet::YUV444_8;
        c.prefs.yuv444 = true;
        assert!(cs.on_client(&c.encode()));
        assert_eq!(cs.wanted(), Want { codec: Codec::Hevc, format: PixelFormat::Yuv444, hdr: false });
        // `--chroma 420` l'emporte.
        cs.configure(ChromaPref::Yuv420, false);
        assert_eq!(cs.wanted().format, PixelFormat::Yuv420);
        // 10 bits préféré + source HDR -> HDR10.
        c.prefs = grx_core::codec_neg::FormatPrefs { yuv444: false, ten_bit: true, hdr_display: false };
        cs.configure(ChromaPref::Auto, true);
        cs.on_client(&c.encode());
        assert_eq!(cs.wanted(), Want { codec: Codec::Hevc, format: PixelFormat::Yuv420P10, hdr: true });
        cs.set_active(cs.wanted(), Some(BackendId::MF_NVIDIA));
        let h = HostCodec::decode(&cs.pending().unwrap().1).unwrap();
        assert_eq!((h.format, h.transfer), (FormatSet::YUV420_10, Transfer::Pq));
        // Échec de l'encodeur 10 bits -> 4:2:0 8 bits, même codec, SDR.
        cs.encoder_failed(cs.wanted());
        assert_eq!(cs.wanted(), Want { codec: Codec::Hevc, format: PixelFormat::Yuv420, hdr: false });
    }

    #[test]
    fn openh264_pins_match_between_loader_and_installer() {
        assert_eq!(grx_update::openh264::DLL_NAME, grx_video::openh264::DLL_NAME);
        assert_eq!(grx_update::openh264::DLL_SHA256, grx_video::openh264::DLL_SHA256);
        assert_eq!(grx_update::openh264::BZ2_SHA256, grx_video::openh264::BZ2_SHA256);
    }

    #[test]
    fn encoder_pref_filters_announced_caps() {
        let list = vec![
            grx_video::EncoderInfo { codec: Codec::Hevc, kind: EncoderKind::Mf(Vendor::Nvidia), name: "n".into(), formats: vec![PixelFormat::Yuv420, PixelFormat::Yuv420P10] },
            grx_video::EncoderInfo { codec: Codec::Hevc, kind: EncoderKind::Nvenc, name: "nv".into(), formats: vec![PixelFormat::Yuv444] },
            grx_video::EncoderInfo { codec: Codec::H264, kind: EncoderKind::Mf(Vendor::Intel), name: "i".into(), formats: vec![PixelFormat::Yuv420] },
            grx_video::EncoderInfo { codec: Codec::H264, kind: EncoderKind::OpenH264, name: "o".into(), formats: vec![PixelFormat::Yuv420] },
        ];
        assert_eq!(caps_from(EncoderPref::Auto, &list).len(), 4);
        let nv = caps_from(EncoderPref::Nvidia, &list);
        assert_eq!(nv.iter().map(|c| c.backend).collect::<Vec<_>>(), vec![BackendId::MF_NVIDIA, BackendId::NVENC]);
        assert_eq!(nv[0].formats, FormatSet::YUV420_8 | FormatSet::YUV420_10);
        assert_eq!(caps_from(EncoderPref::Intel, &list)[0].backend, BackendId::MF_INTEL);
        assert_eq!(caps_from(EncoderPref::Amd, &list).len(), 0);
        let sw = caps_from(EncoderPref::Software, &list);
        assert_eq!((sw.len(), sw[0].backend), (1, BackendId::SOFTWARE));
    }
}
