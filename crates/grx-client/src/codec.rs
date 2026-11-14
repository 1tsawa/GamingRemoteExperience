//! Négociation du codec côté client : sondage des décodeurs réellement utilisables sur le GPU
//! (`grx_render::probe_decoders`, chaîne MF -> FFmpeg D3D11VA), annonce `GRX-COD` au host et
//! renégociation (sans couper la session) quand un décodeur casse en cours de route.

use grx_core::codec_neg::{BackendId, ClientCodecs, CodecCap, CodecSet, VideoCodec};
use grx_render::{DecoderKind, ProbeEntry};
use grx_video::Codec;

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

/// Décodeurs du client et codecs devenus inutilisables pendant la session.
pub struct Decoders {
    probe: Vec<ProbeEntry>,
    unusable: CodecSet,
    pub preferred: Option<VideoCodec>,
}

impl Decoders {
    pub fn new(probe: Vec<ProbeEntry>, preferred: Option<VideoCodec>) -> Decoders {
        Decoders { probe, unusable: CodecSet::EMPTY, preferred }
    }

    /// Codecs décodables (sondage réussi et pas tombés en panne depuis).
    pub fn decodable(&self) -> CodecSet {
        self.probe
            .iter()
            .filter(|p| p.kind.is_some())
            .map(|p| to_neg(p.codec))
            .filter(|c| !self.unusable.contains(*c))
            .collect()
    }

    pub fn is_unusable(&self, c: Codec) -> bool {
        self.unusable.contains(to_neg(c))
    }

    pub fn mark_unusable(&mut self, c: Codec) {
        self.unusable = self.unusable.with(to_neg(c));
    }

    /// Annonce `GRX-COD` (décodeurs utilisables, préférés d'abord : ordre du sondage HEVC, H.264, AV1).
    pub fn announce(&self, renegotiate: bool) -> ClientCodecs {
        let decoders = self
            .probe
            .iter()
            .filter(|p| !self.unusable.contains(to_neg(p.codec)))
            .filter_map(|p| p.kind.map(|k| CodecCap::new(to_neg(p.codec), backend(k))))
            .collect();
        ClientCodecs { decoders, preferred: self.preferred, renegotiate, ..Default::default() }
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
                        format!(" [après échec : {}]", p.attempts.iter().map(|a| format!("{} : {}", a.kind.label(), a.error)).collect::<Vec<_>>().join(" ; "))
                    };
                    format!("{:<5} : {} « {} » ({:.0} ms){fallback}", label(p.codec), k.label(), p.backend, p.probe_ms)
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
        }
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
