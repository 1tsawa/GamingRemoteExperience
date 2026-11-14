//! Réglages de session envoyés par le client (overlay « bande passante max » / « FPS max »),
//! appliqués à chaud. Format : [`grx_core::stream_settings`].
//!
//! * Plafond de débit : `Endpoint::set_max_bitrate_bps` (le contrôle de débit ne dépasse plus
//!   `min(plafond, --max-bitrate)`, l'encodeur suit au tour de boucle suivant).
//! * FPS max : `pipeline::Shared::max_fps`, lu par le thread vidéo (jamais au-dessus de `--fps`) ;
//!   le débit encodeur est corrigé pour que le débit réel reste celui visé malgré moins d'images.
//!
//! Autorisé aussi aux clients en lecture seule (ça ne touche qu'à leur propre flux).

use std::sync::atomic::Ordering;

use grx_core::stream_settings::{HostInfo, StreamSettings};
use grx_transport::Endpoint;

use crate::pipeline::{PipelineOpts, Shared};

/// Cadence effective : `min(--fps, plafond client)` (0 = pas de plafond).
pub fn effective_fps(host_fps: u32, cap: u32) -> u32 {
    let host = host_fps.max(1);
    if cap == 0 {
        host
    } else {
        host.min(cap)
    }
}

/// L'encodeur est configuré pour `host_fps` : avec moins d'images, chaque image reçoit
/// `bitrate/host_fps`. Pour garder le débit réel visé on lui donne `kbps * host_fps / eff_fps`.
pub fn scale_kbps(kbps: u32, host_fps: u32, eff_fps: u32) -> u32 {
    let (h, e) = (host_fps.max(1) as u64, eff_fps.clamp(1, host_fps.max(1)) as u64);
    (kbps as u64 * h / e).min(u32::MAX as u64) as u32
}

/// Annonce envoyée au client (début de session et après chaque réglage).
pub fn host_info(ep: &Endpoint, sh: &Shared, o: &PipelineOpts) -> HostInfo {
    let max = ep.bitrate_controller().max_bps();
    let cap = sh.max_fps.load(Ordering::Relaxed);
    HostInfo {
        bitrate_cap_kbps: (max < o.max_bitrate_bps).then_some((max / 1000) as u32),
        fps_cap: (cap != 0).then(|| effective_fps(o.fps, cap)),
        host_max_bitrate_kbps: (o.max_bitrate_bps / 1000).min(u32::MAX as u64) as u32,
        host_fps: o.fps,
    }
}

/// Traite un message `GRX-SET`. Renvoie l'annonce `GRX-HST` à renvoyer, `None` si le message
/// est invalide (ignoré, sans effet).
pub fn apply(msg: &[u8], ep: &mut Endpoint, sh: &Shared, o: &PipelineOpts) -> Option<Vec<u8>> {
    let s = StreamSettings::decode(msg)?;
    ep.set_max_bitrate_bps(s.max_bitrate_kbps.map(|k| k as u64 * 1000));
    sh.max_fps.store(s.max_fps.unwrap_or(0), Ordering::Relaxed);
    sh.target_bps.store(ep.target_bitrate_bps(), Ordering::Relaxed);
    let info = host_info(ep, sh, o);
    println!(
        "  réglages client : débit max {} | FPS max {}",
        info.bitrate_cap_kbps.map_or("illimité".into(), |k| format!("{:.1} Mb/s", k as f64 / 1000.0)),
        info.fps_cap.map_or(format!("{} (host)", o.fps), |f| f.to_string())
    );
    Some(info.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use grx_input::InjectorConfig;
    use grx_transport::bitrate::BitrateConfig;
    use grx_transport::Config;

    fn opts() -> PipelineOpts {
        PipelineOpts {
            codec: grx_core::codec_neg::CodecPref::Force(grx_core::codec_neg::VideoCodec::Hevc),
            fps: 60,
            fill: false,
            audio: false,
            input: false,
            video: false,
            injector: InjectorConfig::default(),
            initial_bitrate_bps: 20_000_000,
            max_bitrate_bps: 50_000_000,
            display: None,
        }
    }

    fn ep() -> Endpoint {
        Endpoint::new(Config {
            bitrate: BitrateConfig { start_bps: 20_000_000, max_bps: 50_000_000, ..Default::default() },
            ..Default::default()
        })
    }

    #[test]
    fn fps_helpers() {
        assert_eq!(effective_fps(60, 0), 60);
        assert_eq!(effective_fps(60, 30), 30);
        assert_eq!(effective_fps(60, 144), 60);
        assert_eq!(effective_fps(0, 0), 1);
        assert_eq!(scale_kbps(10_000, 60, 30), 20_000);
        assert_eq!(scale_kbps(10_000, 60, 60), 10_000);
        assert_eq!(scale_kbps(10_000, 60, 0), 600_000);
        assert_eq!(scale_kbps(u32::MAX, 240, 1), u32::MAX);
    }

    #[test]
    fn settings_are_applied_and_announced() {
        let o = opts();
        let p = Pipeline::start(&o);
        let mut e = ep();
        let set = StreamSettings { max_bitrate_kbps: Some(5_000), max_fps: Some(30) }.encode();
        let reply = apply(&set, &mut e, &p.shared, &o).expect("message valide");
        let info = HostInfo::decode(&reply).unwrap();
        assert_eq!(info.bitrate_cap_kbps, Some(5_000));
        assert_eq!(info.fps_cap, Some(30));
        assert_eq!((info.host_fps, info.host_max_bitrate_kbps), (60, 50_000));
        assert!(e.stats().total_bitrate_bps <= 5_000_000);
        assert!(p.shared.target_bps.load(Ordering::Relaxed) <= 5_000_000);
        assert_eq!(p.shared.max_fps.load(Ordering::Relaxed), 30);

        // Plafond au-dessus des limites du host : borné par le host.
        let set = StreamSettings { max_bitrate_kbps: Some(900_000), max_fps: Some(240) }.encode();
        let info = HostInfo::decode(&apply(&set, &mut e, &p.shared, &o).unwrap()).unwrap();
        assert_eq!(info.bitrate_cap_kbps, None);
        assert_eq!(info.fps_cap, Some(60));

        // Retour à « illimité ».
        let set = StreamSettings::default().encode();
        let info = HostInfo::decode(&apply(&set, &mut e, &p.shared, &o).unwrap()).unwrap();
        assert_eq!((info.bitrate_cap_kbps, info.fps_cap), (None, None));
        assert_eq!(p.shared.max_fps.load(Ordering::Relaxed), 0);
        p.stop();
    }

    #[test]
    fn garbage_is_ignored_without_effect() {
        let o = opts();
        let p = Pipeline::start(&o);
        let mut e = ep();
        let before = e.stats().total_bitrate_bps;
        for m in [&b""[..], b"GRX-SET", b"GRX-SET\x01\x05", b"GRX-BYE", b"GRX-HST\x01\x00", &[0xFF; 300]] {
            assert!(apply(m, &mut e, &p.shared, &o).is_none());
        }
        assert_eq!(e.stats().total_bitrate_bps, before);
        assert_eq!(p.shared.max_fps.load(Ordering::Relaxed), 0);
        p.stop();
    }
}
