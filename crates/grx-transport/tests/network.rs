//! Tests de bout en bout du transport sur liens simulés déterministes.

use grx_transport::bitrate::BitrateConfig;
use grx_transport::sim::{LinkConfig, Pair, Rng};
use grx_transport::video::FecConfig;
use grx_transport::{Config, Endpoint, Event, Micros};

const FRAME_US: Micros = 16_667; // 60 fps
const STEP_US: Micros = 500;

/// Contenu pseudo-aléatoire déterministe d'une image, fonction de son id.
fn frame_bytes(id: u32, len: usize) -> Vec<u8> {
    let mut r = Rng::new(0xF00D ^ id as u64);
    let mut v = vec![0u8; len];
    r.fill(&mut v);
    v
}

fn cfg(fec_permille: u16) -> Config {
    Config {
        adaptive_fec: false,
        fec: FecConfig {
            group_data: 100,
            parity_permille: fec_permille,
            min_parity: if fec_permille > 0 { 1 } else { 0 },
        },
        bitrate: BitrateConfig { start_bps: 200_000_000, max_bps: 400_000_000, ..Default::default() },
        ..Default::default()
    }
}

struct VideoRun {
    sent: u32,
    delivered: Vec<u32>,
    corrupt: u32,
    out_of_order: u32,
    keyframe_events: u32,
    pair: Pair,
}

/// Émet des images de `len` octets à 60 fps pendant `secs` secondes (keyframe au début, toutes
/// les 2 s et à chaque demande) ; vérifie chaque image livrée octet par octet.
fn run_video(loss: f64, fec_permille: u16, secs: u64, len: usize, seed: u64) -> VideoRun {
    let ab = LinkConfig { loss, delay_us: 10_000, jitter_us: 3_000, seed, ..Default::default() };
    let ba = LinkConfig { loss, delay_us: 10_000, jitter_us: 3_000, seed: seed + 1, ..Default::default() };
    let mut p = Pair::new(Endpoint::new(cfg(fec_permille)), Endpoint::new(cfg(fec_permille)), ab, ba);
    let mut r = VideoRun { sent: 0, delivered: vec![], corrupt: 0, out_of_order: 0, keyframe_events: 0, pair: Pair::new(Endpoint::new(cfg(0)), Endpoint::new(cfg(0)), LinkConfig::default(), LinkConfig::default()) };
    let total = secs * 1_000_000;
    let start = p.now;
    let mut next_frame = p.now;
    let mut need_key = true;
    let mut last_id: Option<u32> = None;
    while p.now - start < total + 500_000 {
        if p.now >= next_frame && p.now - start < total {
            let id = r.sent;
            let key = need_key || id % 120 == 0;
            need_key = false;
            let got = p.a.send_video_frame(p.now, &frame_bytes(id, len), key, p.now).unwrap();
            assert_eq!(got, id);
            r.sent += 1;
            next_frame += FRAME_US;
        }
        p.step(STEP_US);
        while let Some(ev) = p.a.poll_event() {
            if ev == Event::KeyframeRequested {
                r.keyframe_events += 1;
                need_key = true;
            }
        }
        while let Some(ev) = p.b.poll_event() {
            if let Event::Video(f) = ev {
                if f.data != frame_bytes(f.id, len) || f.pts_us == 0 {
                    r.corrupt += 1;
                }
                if last_id.is_some_and(|l| f.id <= l) {
                    r.out_of_order += 1;
                }
                last_id = Some(f.id);
                r.delivered.push(f.id);
            }
        }
    }
    r.pair = p;
    r
}

#[test]
fn video_intact_at_0_percent_loss() {
    let r = run_video(0.0, 100, 3, 100_000, 1);
    assert_eq!(r.corrupt, 0);
    assert_eq!(r.out_of_order, 0);
    assert_eq!(r.delivered.len() as u32, r.sent, "toutes les images doivent arriver");
    assert_eq!(r.keyframe_events, 0);
    let s = r.pair.b.stats();
    println!("0% : {} images, rtt={:?}us", r.sent, s.rtt_us);
    assert!(s.rtt_us.is_some());
}

#[test]
fn video_intact_at_5_percent_loss_with_fec() {
    let r = run_video(0.05, 200, 10, 100_000, 7);
    let ratio = r.delivered.len() as f64 / r.sent as f64;
    let b = r.pair.b.stats().video_rx;
    println!(
        "5% + FEC 20% : {}/{} images livrées ({:.1}%), récupérées par FEC={}, perdues={}, deltas écartées={}, demandes IDR={}, paquets perdus={}",
        r.delivered.len(), r.sent, ratio * 100.0, b.frames_recovered, b.frames_lost, b.deltas_dropped,
        b.keyframe_requests, r.pair.ab.stats.lost
    );
    assert_eq!(r.corrupt, 0, "aucune image corrompue ne doit être livrée");
    assert_eq!(r.out_of_order, 0);
    assert!(ratio > 0.97, "FEC insuffisante : {ratio}");
    assert!(b.frames_recovered > r.sent as u64 / 2, "la FEC doit servir sur la plupart des images");
}

#[test]
fn without_fec_5_percent_loss_still_never_delivers_corruption() {
    let r = run_video(0.05, 0, 5, 100_000, 11);
    let ratio = r.delivered.len() as f64 / r.sent as f64;
    println!("5% sans FEC : {}/{} images livrées ({:.1}%), demandes IDR={}", r.delivered.len(), r.sent, ratio * 100.0, r.keyframe_events);
    assert_eq!(r.corrupt, 0);
    assert_eq!(r.out_of_order, 0);
    assert!(ratio < 0.5, "sans FEC la plupart des images de 85 paquets sont incomplètes");
    assert!(r.keyframe_events > 0);
}

#[test]
fn small_frames_survive_bursty_loss_and_reordering() {
    // Rafales (Gilbert-Elliott), réordonnancement, duplication : jamais d'image fausse.
    let mk = |seed| LinkConfig {
        loss: 0.01,
        burst: Some((0.01, 0.3, 0.6)),
        delay_us: 15_000,
        jitter_us: 6_000,
        reorder: 0.05,
        reorder_extra_us: 12_000,
        duplicate: 0.03,
        seed,
        ..Default::default()
    };
    let mut p = Pair::new(Endpoint::new(cfg(250)), Endpoint::new(cfg(250)), mk(5), mk(6));
    let mut sent = 0u32;
    let mut need_key = true;
    let mut delivered = 0u32;
    let mut last: Option<u32> = None;
    let start = p.now;
    let mut next = p.now;
    while p.now - start < 10_000_000 {
        if p.now >= next && p.now - start < 9_500_000 {
            let key = need_key || sent % 60 == 0;
            need_key = false;
            p.a.send_video_frame(p.now, &frame_bytes(sent, 20_000), key, 1).unwrap();
            sent += 1;
            next += FRAME_US;
        }
        p.step(STEP_US);
        while let Some(ev) = p.a.poll_event() {
            if ev == Event::KeyframeRequested {
                need_key = true;
            }
        }
        while let Some(ev) = p.b.poll_event() {
            if let Event::Video(f) = ev {
                assert_eq!(f.data, frame_bytes(f.id, 20_000), "image {} corrompue", f.id);
                assert!(last.is_none_or(|l| f.id > l));
                last = Some(f.id);
                delivered += 1;
            }
        }
    }
    println!("rafales : {delivered}/{sent} images livrées");
    assert!(delivered as f64 > sent as f64 * 0.7);
}

#[test]
fn unrecoverable_frame_is_never_delivered_and_keyframe_is_requested() {
    let mut a = Endpoint::new(cfg(100));
    let mut b = Endpoint::new(cfg(100));
    let mut now: Micros = 1_000_000;
    let drain = |from: &mut Endpoint, to: &mut Endpoint, now: Micros, drop: &dyn Fn(usize) -> bool| {
        let mut i = 0;
        while let Some(p) = from.poll_transmit(now) {
            if !drop(i) {
                to.handle_datagram(now, &p);
            }
            i += 1;
        }
    };

    // Image 0 (clé) : parfaite.
    a.send_video_frame(now, &frame_bytes(0, 50_000), true, 1).unwrap();
    for _ in 0..200 {
        drain(&mut a, &mut b, now, &|_| false);
        now += 1_000;
    }
    assert!(matches!(b.poll_event(), Some(Event::Video(f)) if f.id == 0));

    // Image 1 (delta) : on perd 15 paquets consécutifs alors que la FEC n'en couvre que ~5.
    a.send_video_frame(now, &frame_bytes(1, 50_000), false, 2).unwrap();
    let mut idx = 0;
    for _ in 0..200 {
        while let Some(p) = a.poll_transmit(now) {
            let dropped = (10..25).contains(&idx) && p[0] & 7 == 0;
            idx += 1;
            if !dropped {
                b.handle_datagram(now, &p);
            }
        }
        now += 1_000;
    }
    // Image 2 (delta) complète : ne doit PAS être livrée (références manquantes).
    a.send_video_frame(now, &frame_bytes(2, 50_000), false, 3).unwrap();
    for _ in 0..300 {
        drain(&mut a, &mut b, now, &|_| false);
        // retour b -> a (demande d'IDR, ACK)
        while let Some(p) = b.poll_transmit(now) {
            a.handle_datagram(now, &p);
        }
        now += 1_000;
    }
    while let Some(ev) = b.poll_event() {
        assert!(!matches!(ev, Event::Video(_)), "image incomplète ou sans références livrée : {ev:?}");
    }
    let s = b.stats().video_rx;
    assert_eq!(s.frames_lost, 1);
    assert_eq!(s.deltas_dropped, 1);
    assert!(s.keyframe_requests >= 1);
    // L'émetteur a reçu la demande d'image clé (canal Control fiable).
    let mut got_req = false;
    while let Some(ev) = a.poll_event() {
        got_req |= ev == Event::KeyframeRequested;
    }
    assert!(got_req, "l'émetteur doit recevoir KeyframeRequested");

    // Il envoie une clé : la lecture reprend, intacte.
    a.send_video_frame(now, &frame_bytes(3, 50_000), true, 4).unwrap();
    for _ in 0..200 {
        drain(&mut a, &mut b, now, &|_| false);
        now += 1_000;
    }
    assert!(matches!(b.poll_event(), Some(Event::Video(f)) if f.id == 3 && f.data == frame_bytes(3, 50_000)));
}

fn run_reliable(loss: f64, reorder: f64, dup: f64, n: u32, rate_hz: u64, control: bool, seed: u64) -> (Vec<u8>, Vec<Micros>) {
    let mk = |s| LinkConfig {
        loss,
        delay_us: 15_000,
        jitter_us: 4_000,
        reorder,
        reorder_extra_us: 10_000,
        duplicate: dup,
        seed: s,
        ..Default::default()
    };
    let mut p = Pair::new(Endpoint::new(Config::default()), Endpoint::new(Config::default()), mk(seed), mk(seed + 100));
    let interval = 1_000_000 / rate_hz;
    let start = p.now;
    let mut next = p.now;
    let mut sent = 0u32;
    let mut sent_at: Vec<Micros> = Vec::new();
    let mut got: Vec<u32> = Vec::new();
    let mut lat: Vec<Micros> = Vec::new();
    // Les messages portent leur numéro (u32) + un remplissage.
    while p.now - start < 120_000_000 && (got.len() as u32) < n {
        if sent < n && p.now >= next {
            let mut m = sent.to_le_bytes().to_vec();
            m.extend_from_slice(&[0xAB; 4]);
            if control {
                p.a.send_control(&m).unwrap();
            } else {
                p.a.send_input(&m).unwrap();
            }
            sent_at.push(p.now);
            sent += 1;
            next += interval;
        }
        p.step(500);
        while let Some(ev) = p.b.poll_event() {
            let m = match ev {
                Event::Input(m) if !control => m,
                Event::Control(m) if control => m,
                other => panic!("événement inattendu {other:?}"),
            };
            assert_eq!(&m[4..], &[0xAB; 4]);
            let id = u32::from_le_bytes(m[..4].try_into().unwrap());
            lat.push(p.now - sent_at[id as usize]);
            got.push(id);
        }
    }
    let ok: Vec<u8> = vec![(got == (0..n).collect::<Vec<_>>()) as u8];
    lat.sort_unstable();
    (ok, lat)
}

#[test]
fn input_events_are_delivered_reliably_in_order_under_20_percent_loss() {
    let (ok, lat) = run_reliable(0.20, 0.05, 0.05, 2000, 1000, false, 21);
    let p50 = lat[lat.len() / 2];
    let p99 = lat[lat.len() * 99 / 100];
    println!("input 20% perte : p50={}ms p99={}ms max={}ms", p50 / 1000, p99 / 1000, lat.last().unwrap() / 1000);
    assert_eq!(ok[0], 1, "tous les événements, exactement une fois, dans l'ordre");
    assert_eq!(lat.len(), 2000);
    assert!(p50 < 200_000);
}

#[test]
fn control_messages_are_reliable_too_and_survive_heavy_loss() {
    let (ok, lat) = run_reliable(0.35, 0.1, 0.1, 500, 200, true, 33);
    assert_eq!(ok[0], 1);
    println!("control 35% perte : p99={}ms", lat[lat.len() * 99 / 100] / 1000);
}

#[test]
fn input_survives_total_blackout_then_recovers() {
    let mut p = Pair::new(
        Endpoint::new(Config::default()),
        Endpoint::new(Config::default()),
        LinkConfig { delay_us: 10_000, seed: 1, ..Default::default() },
        LinkConfig { delay_us: 10_000, seed: 2, ..Default::default() },
    );
    for i in 0..50u8 {
        p.a.send_input(&[i]).unwrap();
    }
    // Coupure complète 2 s : tout part dans le vide.
    p.ab.config_mut().loss = 1.0;
    p.ba.config_mut().loss = 1.0;
    p.run_for(2_000_000, 1000);
    assert!(p.b.poll_event().is_none());
    p.ab.config_mut().loss = 0.0;
    p.ba.config_mut().loss = 0.0;
    p.run_for(3_000_000, 1000);
    let got: Vec<u8> = std::iter::from_fn(|| p.b.poll_event())
        .map(|e| match e {
            Event::Input(m) => m[0],
            e => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(got, (0..50).collect::<Vec<u8>>());
    assert_eq!(p.a.reliable_unacked(), (0, 0));
}

#[test]
fn audio_is_best_effort_without_duplicates_or_reordering() {
    let mut p = Pair::new(
        Endpoint::new(Config::default()),
        Endpoint::new(Config::default()),
        LinkConfig { loss: 0.1, delay_us: 10_000, jitter_us: 8_000, reorder: 0.1, reorder_extra_us: 20_000, duplicate: 0.1, seed: 9, ..Default::default() },
        LinkConfig::default(),
    );
    let mut seqs = Vec::new();
    for i in 0..500u64 {
        p.a.send_audio(i * 10_000, &[i as u8; 120]).unwrap();
        p.run_for(10_000, 500);
        while let Some(ev) = p.b.poll_event() {
            if let Event::Audio(a) = ev {
                assert_eq!(a.data, vec![a.pts_us as u8 / 10; 0].into_iter().chain(std::iter::repeat((a.pts_us / 10_000) as u8).take(120)).collect::<Vec<u8>>());
                seqs.push(a.seq);
            }
        }
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "audio strictement croissant");
    assert!(seqs.len() > 380 && seqs.len() < 500, "{}", seqs.len());
}

#[test]
fn rtt_is_estimated_via_ping_pong() {
    let mut p = Pair::new(
        Endpoint::new(Config::default()),
        Endpoint::new(Config::default()),
        LinkConfig { delay_us: 20_000, seed: 1, ..Default::default() },
        LinkConfig { delay_us: 30_000, seed: 2, ..Default::default() },
    );
    p.run_for(3_000_000, 500);
    let r = p.a.rtt_us().unwrap();
    assert!((49_000..=56_000).contains(&r), "rtt={r}");
    let r = p.b.rtt_us().unwrap();
    assert!((49_000..=56_000).contains(&r), "rtt={r}");
}

#[test]
fn pacing_prevents_bursts() {
    let mut e = Endpoint::new(Config {
        bitrate: BitrateConfig { start_bps: 8_000_000, ..Default::default() },
        pacing_factor: 3.0, // 24 Mb/s
        ..Default::default()
    });
    let mut now: Micros = 5_000_000;
    e.send_video_frame(now, &frame_bytes(0, 100_000), true, 0).unwrap();
    let t0 = now;
    let mut times: Vec<Micros> = vec![];
    let mut bytes = 0usize;
    while times.len() < 200 && now - t0 < 2_000_000 {
        while let Some(p) = e.poll_transmit(now) {
            if p[0] & 7 == 0 {
                times.push(now);
                bytes += p.len();
            }
        }
        now += 100;
    }
    let dur = times.last().unwrap() - times[0];
    let mbps = bytes as f64 * 8.0 / dur as f64;
    // Max de paquets dans n'importe quelle fenêtre de 1 ms.
    let max_1ms = times.iter().enumerate().map(|(i, &t)| times[i..].iter().take_while(|&&u| u - t < 1000).count()).max().unwrap();
    println!("pacing : {} paquets en {} us = {:.1} Mb/s, max {} paquets/ms", times.len(), dur, mbps, max_1ms);
    assert!((20.0..=28.0).contains(&mbps), "{mbps}");
    assert!(max_1ms <= 7, "rafale de {max_1ms} paquets en 1 ms");
}

#[test]
fn stale_frames_are_dropped_on_sender_under_congestion() {
    let mut e = Endpoint::new(Config {
        bitrate: BitrateConfig { start_bps: 2_000_000, ..Default::default() },
        ..Default::default()
    });
    let mut now: Micros = 1_000_000;
    // 5 Mb/s de pacing, 150 ms de file max : 100 ko d'image tous les 16 ms sature vite.
    let mut asked = 0;
    for i in 0..60u32 {
        e.send_video_frame(now, &frame_bytes(i, 100_000), i == 0, 0).unwrap();
        while e.poll_transmit(now).is_some() {}
        now += FRAME_US;
        while let Some(ev) = e.poll_event() {
            asked += (ev == Event::KeyframeRequested) as u32;
        }
    }
    let s = e.stats();
    println!("congestion émetteur : {} images jetées, {} demandes d'IDR locales", s.frames_dropped_tx, asked);
    assert!(s.frames_dropped_tx > 0);
    assert!(asked > 0);
    assert!((e.video_queue_bytes() as f64 * 8.0 / s.pacing_rate_bps as f64) < 0.3);
}

// ───────────────────────────── contrôle de débit sur goulot ─────────────────────────────

/// Encodeur factice : produit des images de la taille imposée par le bitrate cible.
fn run_bottleneck(pair: &mut Pair, secs: u64, bitrate: &mut Vec<(u64, u64)>) -> (u64, u64) {
    let start = pair.now;
    let mut next = pair.now;
    let mut id = 0u32;
    let mut need_key = true;
    let mut delivered = 0u64;
    let mut sampled = 0u64;
    while pair.now - start < secs * 1_000_000 {
        if pair.now >= next {
            let target = pair.a.target_bitrate_bps().max(300_000);
            let size = (target / 8 / 60) as usize;
            let key = need_key;
            need_key = false;
            let _ = pair.a.send_video_frame(pair.now, &frame_bytes(id, size.max(500)), key, 1);
            id += 1;
            next += FRAME_US;
        }
        pair.step(STEP_US);
        while let Some(ev) = pair.a.poll_event() {
            if ev == Event::KeyframeRequested {
                need_key = true;
            }
        }
        while let Some(ev) = pair.b.poll_event() {
            if let Event::Video(_) = ev {
                delivered += 1;
            }
        }
        if pair.now - start >= sampled * 1_000_000 {
            bitrate.push((sampled, pair.a.target_bitrate_bps()));
            sampled += 1;
        }
    }
    (id as u64, delivered)
}

#[test]
fn bitrate_controller_adapts_to_bottleneck_and_recovers() {
    let mk = |rate: u64, seed| LinkConfig {
        delay_us: 20_000,
        jitter_us: 1_000,
        rate_bps: Some(rate),
        queue_bytes: 100_000,
        seed,
        ..Default::default()
    };
    let ccfg = || Config {
        bitrate: BitrateConfig { start_bps: 40_000_000, max_bps: 100_000_000, ..Default::default() },
        ..Default::default()
    };
    let mut p = Pair::new(Endpoint::new(ccfg()), Endpoint::new(ccfg()), mk(20_000_000, 1), LinkConfig { delay_us: 20_000, seed: 2, ..Default::default() });
    let mut hist = Vec::new();

    // Phase 1 : départ à 40 Mb/s sur un lien de 20 Mb/s -> doit redescendre et se stabiliser.
    let (sent, delivered) = run_bottleneck(&mut p, 25, &mut hist);
    let t = p.a.target_bitrate_bps();
    println!("phase 1 (lien 20 Mb/s) : cible finale {:.1} Mb/s, {}/{} images, perte lien={} file={}",
        t as f64 / 1e6, delivered, sent, p.ab.stats.lost, p.ab.stats.queue_dropped);
    println!("  trajectoire (s, Mb/s) : {:?}", hist.iter().step_by(3).map(|(s, b)| (*s, (*b as f64 / 1e5).round() / 10.0)).collect::<Vec<_>>());
    assert!(t > 8_000_000 && t < 21_000_000, "cible hors plage : {t}");

    // Phase 2 : le lien tombe à 6 Mb/s.
    p.ab.config_mut().rate_bps = Some(6_000_000);
    let mut hist2 = Vec::new();
    run_bottleneck(&mut p, 15, &mut hist2);
    let t2 = p.a.target_bitrate_bps();
    println!("phase 2 (lien 6 Mb/s) : cible finale {:.1} Mb/s", t2 as f64 / 1e6);
    assert!(t2 < 6_500_000 && t2 > 1_500_000, "{t2}");

    // Phase 3 : le lien remonte à 50 Mb/s : la cible doit remonter franchement.
    p.ab.config_mut().rate_bps = Some(50_000_000);
    let mut hist3 = Vec::new();
    run_bottleneck(&mut p, 30, &mut hist3);
    let t3 = p.a.target_bitrate_bps();
    println!("phase 3 (lien 50 Mb/s) : cible finale {:.1} Mb/s", t3 as f64 / 1e6);
    assert!(t3 > t2 * 2, "{t2} -> {t3}");
}

// ───────────────────────────── fuzz : paquets forgés ─────────────────────────────

#[test]
fn forged_packets_never_panic() {
    let mut rng = Rng::new(0xFEED);
    let mut ep = Endpoint::new(Config::default());
    let mut now: Micros = 1_000_000;

    // 1. Octets purement aléatoires, toutes longueurs.
    for i in 0..200_000u32 {
        let len = match i % 4 {
            0 => rng.below(8),
            1 => rng.below(40),
            2 => rng.below(200),
            _ => rng.below(1400),
        } as usize;
        let mut b = vec![0u8; len];
        rng.fill(&mut b);
        // Force souvent un `kind` valide pour atteindre les analyseurs.
        if !b.is_empty() && i % 3 != 0 {
            b[0] = (b[0] & !7) | (rng.below(6) as u8);
        }
        ep.handle_datagram(now, &b);
        now += 10;
        if i % 50 == 0 {
            while ep.poll_transmit(now).is_some() {}
            while ep.poll_event().is_some() {}
        }
    }

    // 2. Paquets légitimes (vidéo, audio, fiable, ack, ping, rapport) mutés et tronqués.
    let mut src = Endpoint::new(Config::default());
    let mut dst = Endpoint::new(Config::default());
    src.send_video_frame(now, &frame_bytes(1, 30_000), true, 5).unwrap();
    src.send_audio(1, &[1; 100]).unwrap();
    src.send_input(&[9; 10]).unwrap();
    src.send_control(&[8; 10]).unwrap();
    src.request_keyframe().unwrap();
    let mut legit = Vec::new();
    for _ in 0..500 {
        while let Some(p) = src.poll_transmit(now) {
            dst.handle_datagram(now, &p);
            legit.push(p);
        }
        while let Some(p) = dst.poll_transmit(now) {
            legit.push(p);
        }
        now += 5_000;
    }
    assert!(legit.len() > 30);
    for round in 0..300_000u32 {
        let mut p = legit[rng.below(legit.len() as u64) as usize].clone();
        match round % 4 {
            0 => {
                let n = 1 + rng.below(4);
                for _ in 0..n {
                    let i = rng.below(p.len() as u64) as usize;
                    p[i] ^= 1 << rng.below(8);
                }
            }
            1 => p.truncate(rng.below(p.len() as u64 + 1) as usize),
            2 => {
                // Champs d'en-tête vidéo extrêmes.
                let i = rng.below(p.len().min(19) as u64) as usize;
                p[i] = [0u8, 1, 0x7f, 0x80, 0xff][rng.below(5) as usize];
            }
            _ => {
                let extra = rng.below(50) as usize;
                p.extend(std::iter::repeat(0xff).take(extra));
            }
        }
        ep.handle_datagram(now, &p);
        now += 7;
        if round % 100 == 0 {
            while ep.poll_transmit(now).is_some() {}
            while ep.poll_event().is_some() {}
            let _ = ep.next_wakeup(now);
        }
    }

    // 3. Après tout ça, le pair fonctionne encore et livre une image saine.
    let mut a = Endpoint::new(Config::default());
    let mut b = ep;
    now += 1_000_000;
    a.send_video_frame(now, &frame_bytes(777, 10_000), true, 1).unwrap();
    // Remet les compteurs de `b` à l'heure : on ignore les anciennes images (ids) en
    // envoyant des images successives jusqu'à ce qu'une clé passe.
    let mut ok = false;
    for i in 0..3000u32 {
        while let Some(p) = a.poll_transmit(now) {
            b.handle_datagram(now, &p);
        }
        while let Some(ev) = b.poll_event() {
            if matches!(ev, Event::Video(_)) {
                ok = true;
            }
        }
        if ok {
            break;
        }
        now += 1_000;
        if i % 100 == 99 {
            a.send_video_frame(now, &frame_bytes(777 + i, 10_000), true, 1).unwrap();
        }
    }
    let st = b.stats();
    println!("fuzz : {} paquets rejetés/{} reçus", st.packets_invalid, st.packets_rx);
    assert!(st.packets_invalid > 1000);
}

#[test]
fn forged_video_headers_exhaustive_edges_do_not_panic() {
    use grx_transport::wire::VideoHdr;
    let mut ep = Endpoint::new(Config::default());
    let vals32 = [0u32, 1, 12, 13, 1199, 1200, 65_535, 1 << 20, 32 << 20, (32 << 20) + 1, u32::MAX];
    let vals16 = [0u16, 1, 63, 64, 1182, 65_535];
    let vals8 = [0u8, 1, 2, 100, 200, 254, 255];
    let mut n = 0u64;
    for &frame_len in &vals32 {
        for &shard_size in &vals16 {
            for &kmax in &vals8 {
                for &m in &vals8 {
                    for &group in &[0u16, 1, 65_535] {
                        for &idx in &[0u8, 1, 99, 255] {
                            for parity in [false, true] {
                                let h = VideoHdr { keyframe: n % 2 == 0, parity, tseq: n as u16, frame_id: (n % 5) as u32, frame_len, shard_size, kmax, m, group, idx };
                                for blen in [0usize, 1, 64, 1182] {
                                    let mut p = Vec::new();
                                    h.write(&mut p);
                                    p.extend(std::iter::repeat(0x5a).take(blen));
                                    ep.handle_datagram(n, &p);
                                    n += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    println!("{n} en-têtes vidéo forgés traités");
    while ep.poll_event().is_some() {}
}

#[test]
fn forged_encrypted_packets_are_rejected_by_session_before_transport() {
    use grx_core::{ClientHandshake, HostAuth, Identity, TrustStore};
    let mut host = HostAuth::new(Identity::generate(), TrustStore::in_memory(), "h");
    let client = Identity::generate();
    let now_ms = 1_700_000_000_000u64;
    let tok = host.trust.create_invite(60_000, now_ms).unwrap();
    let (hs, init) = ClientHandshake::start(&client, host.identity().public(), "c", Some(&tok), now_ms).unwrap();
    let (resp, mut hsess) = host.accept(&init, now_ms).unwrap();
    let mut csess = hs.finish(&resp).unwrap();

    let mut a = Endpoint::new(Config::default());
    a.send_input(b"touche").unwrap();
    let payload = a.poll_transmit(0).unwrap();
    let sealed = csess.seal(&payload).unwrap();
    let mut rng = Rng::new(5);
    for _ in 0..5000 {
        let mut f = sealed.clone();
        let i = rng.below(f.len() as u64) as usize;
        f[i] ^= 1 << rng.below(8);
        assert!(hsess.open(&f).is_err());
        let mut r = vec![0u8; rng.below(200) as usize];
        rng.fill(&mut r);
        assert!(hsess.open(&r).is_err());
    }
    assert_eq!(hsess.open(&sealed).unwrap(), payload);
}
