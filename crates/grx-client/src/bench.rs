//! `grx-client bench` : banc d'essai headless de la chaîne complète host -> client.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grx_core::{Identity, Invite};
use grx_input::InputEvent;
use grx_video::nal::{contains_idr, contains_parameter_sets, nal_types};
use grx_video::Codec;

use crate::net::{unix_micros, ClientConnection, ConnectOptions};
use crate::CodecArg;

pub struct BenchOpts {
    pub secs: f64,
    pub save: Option<PathBuf>,
    pub codec: CodecArg,
    pub input: bool,
    pub keyframe_request: bool,
    pub quiet: bool,
}

/// Diagnostic d'une image : `None` = plausible, sinon la raison.
fn validate(codec: Codec, key: bool, d: &[u8]) -> Option<&'static str> {
    if codec == Codec::Av1 {
        if !grx_video::obu::looks_like_av1(d) {
            return Some("flux d'OBU AV1 invalide");
        }
        return (key != grx_video::obu::is_key_frame(d)).then_some("drapeau clé incohérent avec le flux AV1");
    }
    if !(d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])) {
        return Some("pas de start code Annex-B");
    }
    let types = nal_types(codec, d);
    if types.is_empty() {
        return Some("aucune NAL");
    }
    let (vcl_max, valid): (u8, fn(u8) -> bool) = match codec {
        Codec::Hevc => (21, |t| t <= 9 || (16..=21).contains(&t) || (32..=40).contains(&t)),
        Codec::H264 | Codec::Av1 => (5, |t| (1..=12).contains(&t)),
    };
    if types.iter().any(|&t| !valid(t)) {
        return Some("type de NAL invalide");
    }
    let has_vcl = types.iter().any(|&t| match codec {
        Codec::Hevc => t <= vcl_max,
        Codec::H264 | Codec::Av1 => (1..=vcl_max).contains(&t),
    });
    if !has_vcl {
        return Some("aucune slice (VCL)");
    }
    let idr = contains_idr(codec, d);
    if key != idr {
        return Some(if key { "image annoncée clé sans IDR" } else { "IDR dans une image delta" });
    }
    if key && !contains_parameter_sets(codec, d) {
        return Some("image clé sans VPS/SPS/PPS");
    }
    None
}

fn pct(v: &[u64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
}

/// Retourne `true` si le banc est sain (images reçues, toutes plausibles).
pub fn run(
    id: &Identity,
    invite: &Invite,
    opts: ConnectOptions,
    b: BenchOpts,
    on_connected: impl FnOnce(&ClientConnection) -> Result<()>,
) -> Result<bool> {
    let codec = match b.codec {
        CodecArg::Hevc => Codec::Hevc,
        CodecArg::H264 => Codec::H264,
        CodecArg::Av1 => Codec::Av1,
    };
    let t_conn = Instant::now();
    let conn = ClientConnection::connect(id, invite, opts)?;
    println!(
        "Authentifié auprès de « {} » ({}), session {:08x} (poignée de main {:.0} ms)",
        conn.host_name(),
        conn.host_fingerprint(),
        conn.session_id(),
        t_conn.elapsed().as_secs_f64() * 1e3
    );
    on_connected(&conn)?;
    // Négociation : on demande au host le codec attendu (sinon un host en `auto` enverrait du H.264).
    {
        use grx_core::codec_neg::{BackendId, ClientCodecs, CodecSet};
        let c = crate::codec::to_neg(codec);
        conn.send_control(&ClientCodecs::simple(CodecSet::EMPTY.with(c), BackendId::MEDIA_FOUNDATION, Some(c)).encode());
    }
    let mut out = match &b.save {
        Some(p) => Some(BufWriter::new(File::create(p).with_context(|| format!("création de {}", p.display()))?)),
        None => None,
    };
    if b.keyframe_request {
        conn.request_keyframe();
    }

    let start = Instant::now();
    let run_for = Duration::from_secs_f64(b.secs);
    let mut next_input = start;
    let mut input_tick = 0u64;
    let mut next_sec = start + Duration::from_secs(1);

    let (mut frames, mut keyframes, mut bytes) = (0u64, 0u64, 0u64);
    let mut invalid = 0u64;
    let mut invalid_reasons: Vec<(&'static str, u64)> = Vec::new();
    let mut first_frame_ms: Option<f64> = None;
    let (mut first_id, mut last_id) = (None::<u32>, 0u32);
    let mut lat: Vec<u64> = Vec::new();
    let mut per_sec_frames: Vec<u64> = Vec::new();
    let (mut sec_frames, mut sec_bytes, mut sec_lat_sum, mut sec_lat_n) = (0u64, 0u64, 0u64, 0u64);
    let mut last_stats_bytes = 0u64;
    let mut loss_samples: Vec<f32> = Vec::new();
    let (mut audio_pkts, mut audio_bytes, mut audio_gaps, mut audio_last) = (0u64, 0u64, 0u64, None::<u16>);
    let mut audio_lat: Vec<u64> = Vec::new();

    while start.elapsed() < run_for && conn.is_connected() {
        // Entrées inoffensives à 100 Hz.
        if b.input {
            let now = Instant::now();
            while next_input <= now {
                conn.send_input(&InputEvent::MouseMove { dx: 0, dy: 0 }.encode());
                input_tick += 1;
                if input_tick % 100 == 1 {
                    // F24 (scancode 0x76) enfoncée puis relâchée une fois par seconde.
                    conn.send_input(&InputEvent::Key { scancode: 0x76, extended: false, pressed: true }.encode());
                    conn.send_input(&InputEvent::Key { scancode: 0x76, extended: false, pressed: false }.encode());
                }
                if input_tick % 100 == 50 {
                    conn.send_input(&InputEvent::Wheel { horizontal: 0, vertical: 0 }.encode());
                }
                next_input += Duration::from_millis(10);
            }
        }

        let wait = if b.input { next_input.saturating_duration_since(Instant::now()).min(Duration::from_millis(5)) } else { Duration::from_millis(5) };
        let mut got = conn.recv_video_timeout(wait);
        while let Some(f) = got {
            let rx = unix_micros();
            first_frame_ms.get_or_insert(start.elapsed().as_secs_f64() * 1e3);
            frames += 1;
            keyframes += f.keyframe as u64;
            bytes += f.data.len() as u64;
            sec_frames += 1;
            sec_bytes += f.data.len() as u64;
            first_id.get_or_insert(f.id);
            last_id = f.id;
            let l = rx.saturating_sub(f.pts_us);
            lat.push(l);
            sec_lat_sum += l;
            sec_lat_n += 1;
            if let Some(why) = validate(codec, f.keyframe, &f.data) {
                invalid += 1;
                match invalid_reasons.iter_mut().find(|(r, _)| *r == why) {
                    Some((_, n)) => *n += 1,
                    None => invalid_reasons.push((why, 1)),
                }
                if invalid <= 3 {
                    eprintln!("! image {} invalide : {why} ({} octets)", f.id, f.data.len());
                }
            }
            if let Some(w) = out.as_mut() {
                w.write_all(&f.data)?;
            }
            got = conn.poll_video();
        }
        while let Some(a) = conn.poll_audio() {
            audio_pkts += 1;
            audio_bytes += a.data.len() as u64;
            if let Some(l) = audio_last {
                audio_gaps += a.seq.wrapping_sub(l).wrapping_sub(1) as u64 & 0x7FFF;
            }
            audio_last = Some(a.seq);
            audio_lat.push(unix_micros().saturating_sub(a.pts_us));
        }

        if Instant::now() >= next_sec {
            next_sec += Duration::from_secs(1);
            let s = conn.stats();
            let wire = s.transport.bytes_rx - last_stats_bytes;
            last_stats_bytes = s.transport.bytes_rx;
            loss_samples.push(s.transport.rx_loss);
            if !b.quiet {
                println!(
                    "t={:>2}s  {:>3} img/s  {:>6.1} Mb/s vidéo ({:>6.1} sur le fil)  lat {:>5.1} ms  rtt {:>5.2} ms  perte {:>4.1}%  perdues {}  audio {}",
                    per_sec_frames.len() + 1,
                    sec_frames,
                    sec_bytes as f64 * 8.0 / 1e6,
                    wire as f64 * 8.0 / 1e6,
                    if sec_lat_n > 0 { sec_lat_sum as f64 / sec_lat_n as f64 / 1000.0 } else { 0.0 },
                    s.transport.rtt_us.unwrap_or(0) as f64 / 1000.0,
                    s.transport.rx_loss * 100.0,
                    s.transport.video_rx.frames_lost,
                    audio_pkts,
                );
            }
            per_sec_frames.push(sec_frames);
            sec_frames = 0;
            sec_bytes = 0;
            sec_lat_sum = 0;
            sec_lat_n = 0;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let disconnected = !conn.is_connected();
    let state = conn.state();
    let s = conn.stats();
    conn.close();
    if let Some(mut w) = out {
        w.flush()?;
    }

    lat.sort_unstable();
    audio_lat.sort_unstable();
    let v = s.transport.video_rx;
    let expected = first_id.map(|f| last_id.wrapping_sub(f) as u64 + 1).unwrap_or(0);
    let gaps = expected.saturating_sub(frames);
    let mean_loss = if loss_samples.is_empty() { 0.0 } else { loss_samples.iter().sum::<f32>() / loss_samples.len() as f32 };
    let skip = 1.min(per_sec_frames.len()); // 1re seconde = démarrage encodeur, exclue du min
    let (fmin, fmax) = per_sec_frames.iter().skip(skip).fold((u64::MAX, 0), |(a, b), &x| (a.min(x), b.max(x)));

    println!("\n── Résumé bench ({elapsed:.1} s) ──");
    println!(
        "images reçues      : {frames} ({keyframes} clés) | FPS moyen {:.1} (min/max par seconde hors 1re : {}/{})",
        frames as f64 / elapsed,
        if fmin == u64::MAX { 0 } else { fmin },
        fmax
    );
    println!(
        "débit vidéo        : {:.2} Mb/s utile | {:.2} Mb/s sur le fil (FEC+en-têtes, tous canaux)",
        bytes as f64 * 8.0 / elapsed / 1e6,
        s.transport.bytes_rx as f64 * 8.0 / elapsed / 1e6
    );
    println!(
        "latence capture->réception (horloge locale commune) : p50 {:.1} ms | p95 {:.1} | p99 {:.1} | max {:.1}",
        pct(&lat, 0.5),
        pct(&lat, 0.95),
        pct(&lat, 0.99),
        pct(&lat, 1.0)
    );
    println!(
        "RTT transport      : {:.2} ms (min {:.2} ms)",
        s.transport.rtt_us.unwrap_or(0) as f64 / 1000.0,
        s.transport.min_rtt_us.unwrap_or(0) as f64 / 1000.0
    );
    println!(
        "perte              : paquets {:.2}% (moy. des mesures/s) | images perdues {} | trous d'id {} | deltas écartés {} | récupérées par FEC {} | IDR demandées {} | purges file {}",
        mean_loss * 100.0,
        v.frames_lost,
        gaps,
        v.deltas_dropped,
        v.frames_recovered,
        v.keyframe_requests,
        s.video_overflow_dropped
    );
    if s.sim_dropped_rx + s.sim_dropped_tx > 0 {
        println!("perte simulée      : {} datagrammes jetés en réception, {} en émission", s.sim_dropped_rx, s.sim_dropped_tx);
    }
    println!(
        "validité {:?}   : {} images plausibles, {} invalides{}{}",
        codec,
        frames - invalid,
        invalid,
        if first_frame_ms.is_some() { format!(" | 1re image après {:.0} ms", first_frame_ms.unwrap()) } else { String::new() },
        if invalid_reasons.is_empty() { String::new() } else { format!(" | {invalid_reasons:?}") }
    );
    println!(
        "audio              : {audio_pkts} paquets ({audio_bytes} octets), {audio_gaps} trous de séquence, latence p50 {:.1} ms p99 {:.1} ms",
        pct(&audio_lat, 0.5),
        pct(&audio_lat, 0.99)
    );
    println!(
        "entrées            : {} envoyées, {} non acquittées à la fin, {} retransmissions",
        s.input_sent, s.input_unacked, s.transport.input_retransmits
    );
    if let Some(p) = &b.save {
        println!("flux enregistré    : {} ({} octets)", p.display(), bytes);
    }
    if disconnected {
        println!("! connexion fermée avant la fin : {state:?}");
    }
    Ok(frames > 0 && invalid == 0 && !disconnected)
}
