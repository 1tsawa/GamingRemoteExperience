//! Débit du chemin complet : fragmentation + FEC + pacing + chiffrement AES-GCM (grx_core::Session)
//! + déchiffrement + réassemblage (+ décodage FEC si pertes), sur un seul cœur.
//!
//! Le test mesure combien de secondes CPU coûte 1 s de flux à 100 Mb/s. En build release il exige
//! un facteur temps réel > 1 (un seul thread tient 100 Mb/s) ; en debug il vérifie seulement
//! l'exactitude (le code non optimisé est 10 à 30x plus lent).
//! Lancer : `cargo test --release -p grx-transport --test throughput -- --nocapture`

use std::time::{Duration, Instant};

use grx_core::{ClientHandshake, HostAuth, Identity, Session, TrustStore};
use grx_transport::bitrate::BitrateConfig;
use grx_transport::sim::Rng;
use grx_transport::{Config, Endpoint, Event, Micros};

fn sessions() -> (Session, Session) {
    let mut host = HostAuth::new(Identity::generate(), TrustStore::in_memory(), "h");
    let client = Identity::generate();
    let t = 1_700_000_000_000u64;
    let tok = host.trust.create_invite(60_000, t).unwrap();
    let (hs, init) = ClientHandshake::start(&client, host.identity().public(), "c", Some(&tok), t).unwrap();
    let (resp, host_sess) = host.accept(&init, t).unwrap();
    (host_sess, hs.finish(&resp).unwrap())
}

struct Result {
    wall: Duration,
    t_send: Duration,
    t_seal: Duration,
    t_open: Duration,
    frames_ok: u32,
    frames: u32,
    wire_bytes: u64,
    media_bytes: u64,
    recovered: u64,
}

fn run(target_mbps: u64, loss: f64, virtual_secs: u64) -> Result {
    let (mut hs, mut cs) = sessions();
    let cfg = || Config {
        bitrate: BitrateConfig {
            start_bps: target_mbps * 1_000_000 * 12 / 10,
            max_bps: 1_000_000_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = Endpoint::new(cfg());
    let mut client = Endpoint::new(cfg());
    let frame_len = (target_mbps * 1_000_000 / 8 / 60) as usize;
    let mut rng = Rng::new(99);
    let mut payload = vec![0u8; frame_len];
    rng.fill(&mut payload);

    let mut r = Result {
        wall: Duration::ZERO,
        t_send: Duration::ZERO,
        t_seal: Duration::ZERO,
        t_open: Duration::ZERO,
        frames_ok: 0,
        frames: 0,
        wire_bytes: 0,
        media_bytes: 0,
        recovered: 0,
    };
    let frame_us: Micros = 16_667;
    let step: Micros = 250;
    let mut now: Micros = 1_000_000;
    let end = now + virtual_secs * 1_000_000;
    let mut next_frame = now;
    let t_all = Instant::now();
    while now < end {
        if now >= next_frame {
            // Chaque image a un contenu différent (un octet change) pour éviter tout biais.
            payload[0] = r.frames as u8;
            let t = Instant::now();
            host.send_video_frame(now, &payload, r.frames % 120 == 0, now).unwrap();
            r.t_send += t.elapsed();
            r.frames += 1;
            r.media_bytes += frame_len as u64;
            next_frame += frame_us;
        }
        // host -> client (chiffré)
        loop {
            let Some(p) = host.poll_transmit(now) else { break };
            let t = Instant::now();
            let sealed = hs.seal(&p).unwrap();
            r.t_seal += t.elapsed();
            r.wire_bytes += sealed.len() as u64 + 28; // + en-têtes IP/UDP
            if loss > 0.0 && rng.f64() < loss {
                continue;
            }
            let t = Instant::now();
            let opened = cs.open(&sealed).unwrap();
            client.handle_datagram(now, &opened);
            r.t_open += t.elapsed();
        }
        // client -> host (ACK, rapports, pings)
        while let Some(p) = client.poll_transmit(now) {
            let sealed = cs.seal(&p).unwrap();
            let opened = hs.open(&sealed).unwrap();
            host.handle_datagram(now, &opened);
        }
        while let Some(ev) = client.poll_event() {
            if let Event::Video(f) = ev {
                assert_eq!(f.data.len(), frame_len);
                assert_eq!(&f.data[1..], &payload[1..]);
                r.frames_ok += 1;
            }
        }
        while host.poll_event().is_some() {}
        now += step;
    }
    r.wall = t_all.elapsed();
    r.recovered = client.stats().video_rx.frames_recovered;
    r
}

fn report(name: &str, mbps: u64, secs: u64, r: &Result) {
    let virt = secs as f64;
    let wall = r.wall.as_secs_f64();
    let cpu_per_s = wall / virt;
    println!(
        "{name}: flux {mbps} Mb/s ({:.1} Mb/s sur le fil, FEC+en-têtes) pendant {secs}s virtuelles -> {:.2}s réelles | \
         facteur temps réel {:.1}x | CPU {:.1}% d'un coeur | débit max 1 coeur ≈ {:.0} Mb/s | \
         images {}/{} (FEC: {}) | envoi+FEC {:.1} ms/s, chiffrement {:.1} ms/s, déchiffrement+réassemblage {:.1} ms/s",
        r.wire_bytes as f64 * 8.0 / virt / 1e6,
        wall,
        virt / wall,
        cpu_per_s * 100.0,
        mbps as f64 / cpu_per_s,
        r.frames_ok,
        r.frames,
        r.recovered,
        r.t_send.as_secs_f64() / virt * 1e3,
        r.t_seal.as_secs_f64() / virt * 1e3,
        r.t_open.as_secs_f64() / virt * 1e3,
    );
}

#[test]
fn path_sustains_100_mbps_on_one_core() {
    let secs = 5;
    let r = run(100, 0.0, secs);
    report("sans perte", 100, secs, &r);
    // Tolérance de 2 images : la dernière peut être encore en vol à la fin.
    assert!(r.frames_ok + 2 >= r.frames, "{}/{}", r.frames_ok, r.frames);
    if !cfg!(debug_assertions) {
        assert!(r.wall.as_secs_f64() < secs as f64, "ne tient pas 100 Mb/s en temps réel : {:?}", r.wall);
    }
}

#[test]
fn path_sustains_100_mbps_with_5_percent_loss_and_fec_decoding() {
    let secs = 5;
    let r = run(100, 0.05, secs);
    report("5% de perte", 100, secs, &r);
    assert!(r.recovered > 0);
    assert!(r.frames_ok as f64 > r.frames as f64 * 0.9, "{}/{}", r.frames_ok, r.frames);
    if !cfg!(debug_assertions) {
        assert!(r.wall.as_secs_f64() < secs as f64, "{:?}", r.wall);
    }
}
