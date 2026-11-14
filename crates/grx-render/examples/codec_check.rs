//! Vérifie chaque couple (codec, voie de décodage) sans fenêtre : encode des images synthétiques
//! avec le MFT NVIDIA puis les décode, et mesure le temps de décodage (appel + fin du GPU).
//! Usage : codec_check [--disable mf-hevc,...] [--frames N] [--size 1920x1080]
//! Les DLL FFmpeg LGPL sont cherchées dans %GRX_FFMPEG_DIR%, à côté de l'exe, puis exe\ffmpeg.
use grx_render::*;
use grx_video::{Codec, Encoder, EncoderConfig};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let disabled = DisabledDecoders::parse(&get("--disable").unwrap_or_default()).map_err(anyhow::Error::msg)?;
    let frames: usize = get("--frames").and_then(|s| s.parse().ok()).unwrap_or(240);
    let (w, h) = get("--size")
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((1920u32, 1080u32));

    println!("FFmpeg : {:?}", ffmpeg_availability());
    println!("Encodeurs : {:?}", grx_video::available_encoders());
    let device = create_device()?;
    let t0 = Instant::now();
    let probe = probe_decoders(&device, &disabled);
    println!("Sondage : {:.1} ms (2e appel en cache : ", t0.elapsed().as_secs_f64() * 1000.0);
    let t1 = Instant::now();
    let _ = probe_decoders(&device, &disabled);
    println!("  {:.3} ms)", t1.elapsed().as_secs_f64() * 1000.0);
    for p in &probe {
        println!(
            "  {:?}: {:?} « {} » ({:.1} ms) échecs={:?}",
            p.codec, p.kind, p.backend, p.probe_ms, p.attempts
        );
    }

    for codec in [Codec::Hevc, Codec::H264, Codec::Av1] {
        let mut enc = match Encoder::new(EncoderConfig { codec, width: w, height: h, fps: 60, bitrate_kbps: 20_000 }) {
            Ok(e) => e,
            Err(e) => {
                println!("{codec:?} : pas d'encodeur ({e})");
                continue;
            }
        };
        // Images synthétiques qui bougent (sinon l'encodeur produit des images vides).
        let mut packets = Vec::new();
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for i in 0..frames {
            for y in (0..h).step_by(4) {
                let row = (y * w * 4) as usize;
                for x in (0..w).step_by(4) {
                    let o = row + (x * 4) as usize;
                    let v = ((x + y + i as u32 * 7) & 0xFF) as u8;
                    bgra[o] = v;
                    bgra[o + 1] = v.wrapping_mul(3);
                    bgra[o + 2] = (i as u8).wrapping_add((y & 0xFF) as u8);
                }
            }
            let f = enc.encode_bgra(&bgra, w, h, w * 4)?;
            if i == 0 {
                println!(
                    "{codec:?} : 1re image {} o, clé={} accès aléatoire={} sniff={:?}",
                    f.data.len(),
                    f.is_keyframe,
                    contains_random_access(codec, &f.data),
                    sniff_codec(&f.data)
                );
            }
            packets.push(f);
        }
        for kind in DecoderKind::CHAIN {
            if disabled.is_disabled(kind, codec) {
                println!("  {codec:?} / {} : désactivé", kind.label());
                continue;
            }
            let mut dec = match VideoDecoder::with_kind(&device, codec, kind) {
                Ok(d) => d,
                Err(e) => {
                    println!("  {codec:?} / {} : indisponible ({e})", kind.label());
                    continue;
                }
            };
            let (mut call, mut total, mut out) = (Vec::new(), Vec::new(), 0usize);
            for p in &packets {
                let t = Instant::now();
                let r = dec.decode_frame(p);
                let c = t.elapsed().as_secs_f64() * 1000.0;
                dec.sync_gpu()?;
                let tt = t.elapsed().as_secs_f64() * 1000.0;
                match r {
                    Ok(Some(f)) => {
                        if out == 0 {
                            let mut d = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                            unsafe { f.texture.GetDesc(&mut d) };
                            println!(
                                "  {codec:?} / {} : 1re image {}x{} tex {}x{} {:?} array={} bind={:#x}",
                                kind.label(), f.width, f.height, d.Width, d.Height, d.Format, d.ArraySize, d.BindFlags
                            );
                        }
                        out += 1;
                        call.push(c);
                        total.push(tt);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        println!("  {codec:?} / {} : erreur {e}", kind.label());
                        break;
                    }
                }
            }
            let stat = |v: &mut Vec<f64>| {
                v.drain(..v.len().min(10)); // échauffement
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                if v.is_empty() {
                    return (0.0, 0.0);
                }
                (v.iter().sum::<f64>() / v.len() as f64, v[(v.len() * 99 / 100).min(v.len() - 1)])
            };
            let (ca, cp) = stat(&mut call);
            let (ta, tp) = stat(&mut total);
            println!(
                "  {codec:?} / {} « {} » : {out}/{} images ; appel moy {ca:.2} ms (p99 {cp:.2}) ; appel+GPU moy {ta:.2} ms (p99 {tp:.2})",
                kind.label(),
                dec.backend_name(),
                packets.len()
            );
        }
    }
    Ok(())
}
