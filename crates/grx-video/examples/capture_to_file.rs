//! Capture l'écran N secondes, encode en H.265/H.264 matériel et écrit un flux Annex-B brut.
//!
//! cargo run -p grx-video --release --example capture_to_file -- [options]
//!   --codec hevc|h264   (défaut hevc)
//!   --seconds N         (défaut 5)
//!   --fps N             cadence max d'encodage ; 0 = pas de limite (défaut 60)
//!   --bitrate KBPS      (défaut 20000)
//!   --out FILE          (défaut capture.h265 / capture.h264)
//!   --scale WxH         redimensionne sur le GPU (ex. 1920x1080)
//!   --fill              ré-encode la dernière image si l'écran est statique (FPS constant)
//!   --idr-every N       demande un IDR toutes les N images (test de request_keyframe)
//!   --output A:O        adaptateur:sortie à capturer (voir --list)
//!   --list              liste adaptateurs/sorties et quitte

use grx_video::*;
use std::io::Write;
use std::time::{Duration, Instant};

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn main() -> anyhow::Result<()> {
    let mut codec = Codec::Hevc;
    let mut seconds = 5.0f64;
    let mut fps = 60u32;
    let mut bitrate = 20_000u32;
    let mut out: Option<String> = None;
    let mut scale: Option<(u32, u32)> = None;
    let mut fill = false;
    let mut idr_every = 0u64;
    let mut output: Option<(u32, u32)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--codec" => {
                codec = match args.next().unwrap_or_default().as_str() {
                    "h264" => Codec::H264,
                    _ => Codec::Hevc,
                }
            }
            "--seconds" => seconds = args.next().unwrap().parse()?,
            "--fps" => fps = args.next().unwrap().parse()?,
            "--bitrate" => bitrate = args.next().unwrap().parse()?,
            "--out" => out = args.next(),
            "--scale" => {
                let s = args.next().unwrap();
                let (w, h) = s.split_once('x').expect("--scale WxH");
                scale = Some((w.parse()?, h.parse()?));
            }
            "--fill" => fill = true,
            "--idr-every" => idr_every = args.next().unwrap().parse()?,
            "--output" => {
                let s = args.next().unwrap();
                let (a, o) = s.split_once(':').expect("--output A:O");
                output = Some((a.parse()?, o.parse()?));
            }
            "--list" => {
                for a in list_adapters()? {
                    println!("adapter {} : {} (vendor {:#06x}, software={})", a.index, a.name, a.vendor_id, a.is_software);
                }
                for o in list_outputs()? {
                    println!(
                        "output {}:{} on '{}' {}x{} attached={} primary={}",
                        o.adapter_index, o.output_index, o.adapter_name, o.width, o.height, o.attached_to_desktop, o.primary
                    );
                }
                return Ok(());
            }
            other => anyhow::bail!("unknown option {other}"),
        }
    }
    let out = out.unwrap_or_else(|| format!("capture.{}", codec.file_extension()));

    let mut cap = match output {
        Some((a, o)) => ScreenCapturer::with_output(a, o)?,
        None => ScreenCapturer::new()?,
    };
    let (sw, sh) = cap.size();
    let (w, h) = scale.unwrap_or((sw, sh));
    let target_fps = if fps == 0 { 60 } else { fps };
    let t_init = Instant::now();
    let mut enc = Encoder::with_device(
        cap.device(),
        EncoderConfig { codec, width: w, height: h, fps: target_fps, bitrate_kbps: bitrate },
    )?;
    println!(
        "capture {sw}x{sh} -> encode {w}x{h} {:?} {} kbps, backend {:?} (init {:.0} ms)",
        codec,
        bitrate,
        enc.backend(),
        t_init.elapsed().as_secs_f64() * 1e3
    );
    for wmsg in enc.warnings() {
        println!("  warning: {wmsg}");
    }

    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);
    // Pacing par échéance : on accepte une frame si on a atteint l'échéance (-1 ms de tolérance).
    let period = if fps == 0 { Duration::ZERO } else { Duration::from_secs_f64(1.0 / fps as f64) };
    let mut next_due = Instant::now();
    let run = Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let mut lat_ms: Vec<f64> = Vec::new(); // capture -> bitstream disponible
    let mut enc_ms: Vec<f64> = Vec::new(); // convert + encode seulement
    let mut frames = 0u64;
    let mut keyframes = 0u64;
    let mut bytes = 0u64;
    let mut dropped = 0u64;
    let mut first_frame_ms = None;
    let mut sizes: Vec<usize> = Vec::new();

    while t0.elapsed() < run {
        let frame = match cap.capture(if fill { (1000 / target_fps.max(1)) as u32 } else { 100 })? {
            Some(f) => f,
            None if fill => match cap.last_frame() {
                Some(f) => f,
                None => continue,
            },
            None => continue,
        };
        let now = Instant::now();
        if now + Duration::from_millis(1) < next_due {
            dropped += 1;
            continue;
        }
        next_due += period;
        if next_due + period < now {
            next_due = now + period; // en retard de plus d'une période : on resynchronise
        }
        if idr_every > 0 && frames > 0 && frames % idr_every == 0 {
            enc.request_keyframe();
        }
        let t_enc = Instant::now();
        let ef = enc.encode(&frame)?;
        let done = Instant::now();
        lat_ms.push((done - frame.captured_at).as_secs_f64() * 1e3);
        enc_ms.push((done - t_enc).as_secs_f64() * 1e3);
        first_frame_ms.get_or_insert(t0.elapsed().as_secs_f64() * 1e3);
        frames += 1;
        keyframes += ef.is_keyframe as u64;
        bytes += ef.data.len() as u64;
        sizes.push(ef.data.len());
        file.write_all(&ef.data)?;
    }
    file.flush()?;
    let el = t0.elapsed().as_secs_f64();
    lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    enc_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    println!("wrote {out}: {frames} frames ({keyframes} IDR), {bytes} bytes, {} dropped for pacing", dropped);
    println!("achieved FPS : {:.1}  (over {:.2}s)", frames as f64 / el, el);
    println!("bitrate      : {:.0} kbps", bytes as f64 * 8.0 / el / 1000.0);
    println!(
        "capture->encoded latency ms : avg {:.2}  p50 {:.2}  p99 {:.2}  max {:.2}",
        avg(&lat_ms),
        pct(&lat_ms, 0.5),
        pct(&lat_ms, 0.99),
        lat_ms.last().copied().unwrap_or(0.0)
    );
    println!(
        "convert+encode only ms      : avg {:.2}  p50 {:.2}  p99 {:.2}",
        avg(&enc_ms),
        pct(&enc_ms, 0.5),
        pct(&enc_ms, 0.99)
    );
    println!("first frame at {:.0} ms; capturer reopened {} time(s)", first_frame_ms.unwrap_or(0.0), cap.reopen_count);
    if let (Some(first), Some(max)) = (sizes.first(), sizes.iter().skip(1).max()) {
        println!("first (IDR) frame {} B, largest later frame {} B", first, max);
    }
    Ok(())
}
