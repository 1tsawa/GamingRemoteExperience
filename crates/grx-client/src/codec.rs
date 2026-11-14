//! Négociation du codec côté client : sondage des décodeurs réellement utilisables sur le GPU
//! (`grx_render::probe_decoders`, chaîne MF -> FFmpeg D3D11VA), annonce `GRX-COD` au host et
//! renégociation (sans couper la session) quand un décodeur casse en cours de route.
//!
//! Phase 2 : chaque voie (matérielle retenue, OpenH264, FFmpeg logiciel) est annoncée avec ses
//! formats (4:2:0/4:4:4, 8/10 bits) ; le choix « Décodeur » (Auto / Matériel / Logiciel, overlay
//! ou `play --decoder`) filtre l'annonce et part dans `DecoderMode` ; les préférences « Couleur
//! 4:4:4 » / « Préférer 10 bits » et l'écran HDR partent dans `FormatPrefs`.

use grx_core::codec_neg::{BackendId, ClientCodecs, CodecCap, CodecSet, DecoderMode, FormatPrefs, FormatSet, VideoCodec};
use grx_render::{DecoderKind, DecoderPref, ProbeEntry};
use grx_video::{Codec, PixelFormat};

pub fn to_neg(c: Codec) -> VideoCodec {
    match c {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
        Codec::Av1 => VideoCodec::Av1,
    }
}

pub fn label(c: Codec) -> &'static str {
    to_neg(c).label()
}

fn backend(k: DecoderKind) -> BackendId {
    BackendId(k.backend_id())
}

pub fn mode_of(p: DecoderPref) -> DecoderMode {
    match p {
        DecoderPref::Auto => DecoderMode::Auto,
        DecoderPref::Hardware => DecoderMode::Hardware,
        DecoderPref::Software => DecoderMode::Software,
    }
}

/// La voie `k` est-elle permise par le choix « Décodeur » ?
pub fn allowed(p: DecoderPref, k: DecoderKind) -> bool {
    match p {
        DecoderPref::Auto => true,
        DecoderPref::Hardware => !k.is_software(),
        DecoderPref::Software => k.is_software(),
    }
}

pub fn formats_bits(v: &[PixelFormat]) -> FormatSet {
    v.iter().fold(FormatSet::default(), |a, f| a.union(FormatSet::single(f.is_444(), f.is_10bit())))
}

/// Décodeurs du client et codecs devenus inutilisables pendant la session.
pub struct Decoders {
    probe: Vec<ProbeEntry>,
    unusable: CodecSet,
    pub preferred: Option<VideoCodec>,
    /// Choix « Décodeur » (Auto / Matériel / Logiciel).
    pub mode: DecoderPref,
    /// « Couleur 4:4:4 », « Préférer 10 bits », écran HDR.
    pub prefs: FormatPrefs,
    /// Formats tombés en panne pendant la session (retirés de l'annonce, le codec reste).
    bad_formats: Vec<(VideoCodec, FormatSet)>,
}

impl Decoders {
    pub fn new(probe: Vec<ProbeEntry>, preferred: Option<VideoCodec>) -> Decoders {
        Decoders {
            probe,
            unusable: CodecSet::EMPTY,
            preferred,
            mode: DecoderPref::Auto,
            prefs: FormatPrefs::default(),
            bad_formats: Vec::new(),
        }
    }

    /// Le format `f` de `c` ne se décode pas en pratique : retiré de l'annonce.
    pub fn mark_format_unusable(&mut self, c: Codec, f: FormatSet) {
        self.bad_formats.push((to_neg(c), f));
    }

    /// Capacités utilisables (voie permise par le mode, codec pas tombé en panne).
    fn caps(&self) -> Vec<CodecCap> {
        let mut v = Vec::new();
        for p in self.probe.iter().filter(|p| !self.unusable.contains(to_neg(p.codec))) {
            if p.caps.is_empty() {
                // Sondage phase 1 (sans détail des formats) : la voie retenue, 4:2:0 8 bits.
                if let Some(k) = p.kind.filter(|k| allowed(self.mode, *k)) {
                    v.push(CodecCap::new(to_neg(p.codec), backend(k)));
                }
                continue;
            }
            let bad = self.bad_formats.iter().filter(|(c, _)| *c == to_neg(p.codec)).fold(FormatSet::default(), |a, (_, f)| a.union(*f));
            for (k, f) in p.caps.iter().filter(|(k, _)| allowed(self.mode, *k)) {
                let formats = FormatSet::from_bits(formats_bits(f).bits() & !bad.bits()).unwrap_or_default();
                if !formats.is_empty() {
                    v.push(CodecCap { codec: to_neg(p.codec), backend: backend(*k), formats });
                }
            }
        }
        v
    }

    /// Codecs décodables (sondage réussi, voie permise et pas tombés en panne depuis).
    pub fn decodable(&self) -> CodecSet {
        self.caps().iter().map(|c| c.codec).collect()
    }

    /// Voies à essayer pour ce codec dans ce format (matérielles d'abord), selon le mode.
    pub fn kinds_for(&self, c: Codec, f: PixelFormat) -> Vec<DecoderKind> {
        let want = FormatSet::single(f.is_444(), f.is_10bit());
        let mut v: Vec<DecoderKind> = self
            .probe
            .iter()
            .filter(|p| p.codec == c)
            .flat_map(|p| p.caps.iter())
            .filter(|(k, fs)| allowed(self.mode, *k) && formats_bits(fs).contains(want))
            .map(|(k, _)| *k)
            .collect();
        v.dedup();
        v
    }

    pub fn is_unusable(&self, c: Codec) -> bool {
        self.unusable.contains(to_neg(c))
    }

    pub fn mark_unusable(&mut self, c: Codec) {
        self.unusable = self.unusable.with(to_neg(c));
    }

    /// Annonce `GRX-COD` (décodeurs utilisables, préférés d'abord : ordre du sondage HEVC, H.264, AV1).
    pub fn announce(&self, renegotiate: bool) -> ClientCodecs {
        ClientCodecs {
            decoders: self.caps(),
            preferred: self.preferred,
            renegotiate,
            decoder_mode: mode_of(self.mode),
            prefs: self.prefs,
        }
    }

    /// Lignes de journal du sondage.
    pub fn summary(&self) -> Vec<String> {
        self.probe
            .iter()
            .map(|p| match (p.kind, p.error()) {
                (Some(k), _) => {
                    let fallback = if p.attempts.is_empty() {
                        String::new()
                    } else {
                        format!(" [échecs : {}]", p.attempts.iter().map(|a| format!("{} : {}", a.kind.label(), a.error)).collect::<Vec<_>>().join(" ; "))
                    };
                    let voies: Vec<String> =
                        p.caps.iter().map(|(k, f)| format!("{} {}", k.label(), formats_bits(f).label())).collect();
                    let voies = if voies.is_empty() { String::new() } else { format!(" ; voies : {}", voies.join(", ")) };
                    format!("{:<5} : {} « {} » ({:.0} ms){voies}{fallback}", label(p.codec), k.label(), p.backend, p.probe_ms)
                }
                (None, Some(e)) => format!("{:<5} : indisponible — {e}", label(p.codec)),
                (None, None) => format!("{:<5} : indisponible", label(p.codec)),
            })
            .collect()
    }
}

/// Message d'erreur final quand aucun décodeur n'est utilisable et qu'aucune renégociation
/// n'est possible (ancien host, ou H.264 lui-même en panne). Pas de conseil réseau : la
/// connexion fonctionne, c'est le décodage qui échoue.
pub fn fatal_message(codec: Codec, cause: &str, host_negotiates: bool) -> String {
    let mut m = format!("Décodeur vidéo {} indisponible sur ce PC ({cause}).", label(codec));
    match codec {
        Codec::H264 => m.push_str(" Solution : mettez à jour le pilote graphique."),
        Codec::Hevc | Codec::Av1 => {
            let ext = if codec == Codec::Hevc {
                "« HEVC Video Extensions from Device Manufacturer »"
            } else {
                "« AV1 Video Extension »"
            };
            m.push_str(&format!(" Solutions : installez l'extension gratuite {ext} (Microsoft Store), ou placez les DLL FFmpeg LGPL à côté de grx-client.exe"));
            if host_negotiates {
                m.push_str(", ou relancez : le host passera en H.264 automatiquement.");
            } else {
                m.push_str(", ou mettez à jour le host (négociation automatique du codec) ou lancez-le avec `--codec h264`.");
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_render::Attempt;

    fn entry(codec: Codec, kind: Option<DecoderKind>) -> ProbeEntry {
        ProbeEntry {
            codec,
            kind,
            backend: "x".into(),
            attempts: if kind.is_none() {
                vec![Attempt { kind: DecoderKind::MediaFoundation, error: "aucun MFT".into() }]
            } else {
                vec![]
            },
            probe_ms: 1.0,
            caps: vec![],
        }
    }

    #[test]
    fn decoder_mode_filters_announcement_and_formats() {
        let mut hevc = entry(Codec::Hevc, Some(DecoderKind::MediaFoundation));
        hevc.caps = vec![
            (DecoderKind::MediaFoundation, vec![PixelFormat::Yuv420, PixelFormat::Yuv420P10, PixelFormat::Yuv444]),
            (DecoderKind::FfmpegSoftware, PixelFormat::ALL.to_vec()),
        ];
        let mut h264 = entry(Codec::H264, Some(DecoderKind::MediaFoundation));
        h264.caps = vec![(DecoderKind::MediaFoundation, vec![PixelFormat::Yuv420]), (DecoderKind::OpenH264, vec![PixelFormat::Yuv420])];
        let mut d = Decoders::new(vec![hevc, h264], None);
        let a = d.announce(false);
        assert_eq!(a.decoder_mode, DecoderMode::Auto);
        assert_eq!(a.decoders.len(), 4);
        assert_eq!(a.formats_for(VideoCodec::Hevc), FormatSet::YUV420_8 | FormatSet::YUV420_10 | FormatSet::YUV444_8 | FormatSet::YUV444_10);
        assert_eq!(d.kinds_for(Codec::Hevc, PixelFormat::Yuv444P10), vec![DecoderKind::FfmpegSoftware]);
        assert_eq!(d.kinds_for(Codec::Hevc, PixelFormat::Yuv444), vec![DecoderKind::MediaFoundation, DecoderKind::FfmpegSoftware]);
        d.mode = DecoderPref::Hardware;
        let a = d.announce(false);
        assert!(a.decoders.iter().all(|c| c.backend != BackendId::SOFTWARE));
        assert_eq!(a.decoder_mode, DecoderMode::Hardware);
        assert!(d.kinds_for(Codec::Hevc, PixelFormat::Yuv444P10).is_empty());
        d.mode = DecoderPref::Software;
        d.prefs.yuv444 = true;
        let a = d.announce(false);
        assert!(a.decoders.iter().all(|c| c.backend == BackendId::SOFTWARE));
        assert_eq!(a.prefs.yuv444, true);
        assert_eq!(ClientCodecs::decode(&a.encode()), Some(a));
        assert_eq!(d.kinds_for(Codec::H264, PixelFormat::Yuv420), vec![DecoderKind::OpenH264]);
        assert!(d.summary()[0].contains("voies"));
    }

    #[test]
    fn intel_pc_without_hevc_announces_h264_and_av1() {
        let d = Decoders::new(
            vec![
                entry(Codec::Hevc, None),
                entry(Codec::H264, Some(DecoderKind::MediaFoundation)),
                entry(Codec::Av1, Some(DecoderKind::MediaFoundation)),
            ],
            None,
        );
        let a = d.announce(false);
        assert_eq!(a.decodable(), [VideoCodec::H264, VideoCodec::Av1].into_iter().collect());
        assert_eq!(ClientCodecs::decode(&a.encode()), Some(a));
        assert!(d.summary()[0].contains("indisponible"));
    }

    #[test]
    fn broken_decoder_is_removed_from_announcement() {
        let mut d = Decoders::new(
            vec![entry(Codec::Hevc, Some(DecoderKind::FfmpegD3d11va)), entry(Codec::H264, Some(DecoderKind::MediaFoundation))],
            Some(VideoCodec::Hevc),
        );
        assert_eq!(d.announce(false).backend_for(VideoCodec::Hevc), Some(BackendId::FFMPEG_D3D11VA));
        d.mark_unusable(Codec::Hevc);
        let a = d.announce(true);
        assert!(a.renegotiate);
        assert_eq!(a.decodable(), CodecSet::EMPTY.with(VideoCodec::H264));
        // Avec la logique du host : H.264.
        let (c, _) = grx_core::codec_neg::choose(
            grx_core::codec_neg::CodecPref::Auto,
            [VideoCodec::H264, VideoCodec::Hevc].into_iter().collect(),
            Some(&a),
        );
        assert_eq!(c, VideoCodec::H264);
    }

    #[test]
    fn fatal_message_has_no_network_advice() {
        for (c, n) in [(Codec::Hevc, true), (Codec::Hevc, false), (Codec::H264, true), (Codec::Av1, false)] {
            let m = fatal_message(c, "aucun MFT", n);
            assert!(!m.contains("pare-feu") && !m.contains("NAT"), "{m}");
            assert!(m.contains(label(c)));
        }
        assert!(fatal_message(Codec::Hevc, "x", true).contains("HEVC Video Extensions"));
        assert!(fatal_message(Codec::Hevc, "x", false).contains("--codec h264"));
    }
}
