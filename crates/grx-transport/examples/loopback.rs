//! Démo de bout en bout sur 127.0.0.1 : poignée de main Noise avec invitation (comme
//! `grx-host` / `grx-client`), puis flux de fausses images de 100 Ko à 60 fps du host vers le
//! client à travers `grx_core::Session` (AES-256-GCM) et `grx_transport::Endpoint`.
//! Le client renvoie des événements d'entrée (125 Hz) sur le canal fiable.
//!
//! Lancer : `cargo run --release -p grx-transport --example loopback -- [--secs 5] [--kb 100]
//!          [--fps 60] [--loss 0.0]`
//! `--loss` injecte une perte aléatoire sur les paquets envoyés par le host ET par le client.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use grx_core::util::now_ms;
use grx_core::{ClientHandshake, HostAuth, Identity, Invite, Session, TrustStore, PKT_DATA, PKT_INIT};
use grx_transport::bitrate::BitrateConfig;
use grx_transport::sim::Rng;
use grx_transport::{Config, Endpoint, Event, Micros};

struct Args {
    secs: u64,
    kb: usize,
    fps: u64,
    loss: f64,
}

fn parse_args() -> Args {
    let mut a = Args { secs: 5, kb: 100, fps: 60, loss: 0.0 };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let val = v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--secs" => a.secs = val.parse().expect("--secs N"),
            "--kb" => a.kb = val.parse().expect("--kb N"),
            "--fps" => a.fps = val.parse().expect("--fps N"),
            "--loss" => a.loss = val.parse().expect("--loss 0.05"),
            other => panic!("argument inconnu : {other}"),
        }
        i += 2;
    }
    a
}

fn config(kb: usize, fps: u64) -> Config {
    let need = (kb as u64 * 1000 * 8 * fps) * 13 / 10; // + FEC et en-têtes
    Config {
        bitrate: BitrateConfig { start_bps: need.max(20_000_000), max_bps: 1_000_000_000, ..Default::default() },
        ..Default::default()
    }
}

fn fake_frame(no: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..4].copy_from_slice(&no.to_le_bytes());
    for (i, b) in v.iter_mut().enumerate().skip(4) {
        *b = (no as usize).wrapping_add(i) as u8;
    }
    v
}

fn frame_ok(f: &[u8], len: usize) -> bool {
    if f.len() != len {
        return false;
    }
    let no = u32::from_le_bytes(f[..4].try_into().unwrap());
    f.iter().enumerate().skip(4).all(|(i, &b)| b == (no as usize).wrapping_add(i) as u8)
}

fn is_reset(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted)
}

/// Un pair : socket + session + endpoint. `peer` est mis à jour à chaque paquet authentifié.
struct Peer {
    sock: UdpSocket,
    sess: Session,
    ep: Endpoint,
    peer: SocketAddr,
    t0: Instant,
    rng: Rng,
    loss: f64,
    buf: Vec<u8>,
    sent_pkts: u64,
    forged_or_rejected: u64,
}

impl Peer {
    fn now_us(&self) -> Micros {
        self.t0.elapsed().as_micros() as Micros
    }

    fn pump(&mut self) {
        let now = self.now_us();
        loop {
            match self.sock.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    if self.buf[0] != PKT_DATA {
                        continue;
                    }
                    match self.sess.open(&self.buf[..n]) {
                        Ok(payload) => {
                            self.peer = from;
                            self.ep.handle_datagram(now, &payload);
                        }
                        Err(_) => self.forged_or_rejected += 1,
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if is_reset(&e) => continue,
                Err(e) => panic!("recv: {e}"),
            }
        }
        while let Some(p) = self.ep.poll_transmit(now) {
            let sealed = self.sess.seal(&p).expect("seal");
            self.sent_pkts += 1;
            if self.loss > 0.0 && self.rng.f64() < self.loss {
                continue; // perte simulée en sortie
            }
            if let Err(e) = self.sock.send_to(&sealed, self.peer) {
                if !is_reset(&e) && e.kind() != ErrorKind::WouldBlock {
                    panic!("send: {e}");
                }
            }
        }
    }
}

struct HostReport {
    frames_sent: u32,
    input_ok: u32,
    input_out_of_order: u32,
    audio_sent: u32,
    stats: grx_transport::Stats,
    sent_pkts: u64,
}

struct ClientReport {
    frames_ok: u32,
    frames_bad: u32,
    audio: u32,
    inputs_sent: u32,
    latencies_us: Vec<u64>,
    per_second: Vec<(u32, u64)>,
    stats: grx_transport::Stats,
    rejected: u64,
}

fn run_host(sock: UdpSocket, mut auth: HostAuth, args: &Args, t0: Instant) -> HostReport {
    // Poignée de main : on attend l'INIT du client.
    let mut buf = vec![0u8; 2048];
    let (n, from) = sock.recv_from(&mut buf).expect("INIT");
    assert_eq!(buf[0], PKT_INIT);
    let (resp, sess) = auth.accept(&buf[..n], now_ms()).expect("accept");
    sock.send_to(&resp, from).unwrap();
    println!("[host] client {} ({}) connecté, session {:08x}", sess.peer_name(), grx_core::fingerprint(&sess.peer_key()), sess.sid());
    sock.set_nonblocking(true).unwrap();

    let mut p = Peer { sock, sess, ep: Endpoint::new(config(args.kb, args.fps)), peer: from, t0, rng: Rng::new(1), loss: args.loss, buf, sent_pkts: 0, forged_or_rejected: 0 };
    let frame_us = 1_000_000 / args.fps;
    let len = args.kb * 1000;
    let start = p.now_us();
    let mut next_frame = start;
    let mut next_audio = start;
    let (mut frames, mut audio, mut need_key) = (0u32, 0u32, true);
    let (mut input_ok, mut input_ooo, mut last_in) = (0u32, 0u32, None::<u32>);
    let end = start + args.secs * 1_000_000 + 700_000;

    loop {
        let now = p.now_us();
        if now >= end {
            break;
        }
        let sending = now < start + args.secs * 1_000_000;
        if sending && now >= next_frame {
            let key = need_key || frames % 120 == 0;
            need_key = false;
            p.ep.send_video_frame(now, &fake_frame(frames, len), key, now).expect("send frame");
            frames += 1;
            next_frame += frame_us;
        }
        if sending && now >= next_audio {
            p.ep.send_audio(now, &[audio as u8; 120]).unwrap();
            audio += 1;
            next_audio += 10_000;
        }
        p.pump();
        while let Some(ev) = p.ep.poll_event() {
            match ev {
                Event::KeyframeRequested => need_key = true,
                Event::Input(m) => {
                    let seq = u32::from_le_bytes(m[..4].try_into().unwrap());
                    match last_in {
                        Some(l) if seq != l + 1 => input_ooo += 1,
                        _ => input_ok += 1,
                    }
                    last_in = Some(seq);
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_micros(150));
    }
    HostReport { frames_sent: frames, input_ok, input_out_of_order: input_ooo, audio_sent: audio, stats: p.ep.stats(), sent_pkts: p.sent_pkts }
}

fn run_client(sock: UdpSocket, inv: Invite, id: Identity, args: &Args, t0: Instant) -> ClientReport {
    let host: SocketAddr = inv.addr.parse().unwrap();
    sock.connect(host).unwrap();
    let (hs, init) = ClientHandshake::start(&id, inv.host_key, "loopback-client", inv.token.as_ref(), now_ms()).unwrap();
    sock.send(&init).unwrap();
    let mut buf = vec![0u8; 2048];
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let n = sock.recv(&mut buf).expect("RESP");
    let sess = hs.finish(&buf[..n]).expect("finish");
    sock.set_read_timeout(None).unwrap();
    sock.set_nonblocking(true).unwrap();
    println!("[client] session {:08x} établie avec le host {}", sess.sid(), grx_core::fingerprint(&sess.peer_key()));

    let mut p = Peer { sock, sess, ep: Endpoint::new(config(args.kb, args.fps)), peer: host, t0, rng: Rng::new(2), loss: args.loss, buf, sent_pkts: 0, forged_or_rejected: 0 };
    let len = args.kb * 1000;
    let start = p.now_us();
    let end = start + args.secs * 1_000_000 + 700_000;
    let mut next_input = start;
    let mut inputs = 0u32;
    let (mut ok, mut bad, mut audio) = (0u32, 0u32, 0u32);
    let mut lat = Vec::new();
    let mut per_second = Vec::new();
    let (mut sec_frames, mut sec_bytes, mut next_sec) = (0u32, 0u64, start + 1_000_000);

    loop {
        let now = p.now_us();
        if now >= end {
            break;
        }
        if now < start + args.secs * 1_000_000 && now >= next_input {
            // « touche enfoncée/relâchée » : 8 octets (numéro + code)
            let mut m = inputs.to_le_bytes().to_vec();
            m.extend_from_slice(&[0x41, (inputs & 1) as u8, 0, 0]);
            p.ep.send_input(&m).unwrap();
            inputs += 1;
            next_input += 8_000;
        }
        p.pump();
        while let Some(ev) = p.ep.poll_event() {
            match ev {
                Event::Video(f) => {
                    if frame_ok(&f.data, len) {
                        ok += 1;
                        sec_frames += 1;
                        sec_bytes += f.data.len() as u64;
                        lat.push(p.now_us().saturating_sub(f.pts_us));
                    } else {
                        bad += 1;
                    }
                }
                Event::Audio(_) => audio += 1,
                _ => {}
            }
        }
        if now >= next_sec {
            let s = p.ep.stats();
            println!(
                "[client] t={:>2}s  {:>3} images/s  {:>6.1} Mb/s utiles  rtt={:>5} us  perte vue={:>4.1}%",
                per_second.len() + 1,
                sec_frames,
                sec_bytes as f64 * 8.0 / 1e6,
                s.rtt_us.unwrap_or(0),
                s.rx_loss * 100.0,
            );
            per_second.push((sec_frames, sec_bytes));
            sec_frames = 0;
            sec_bytes = 0;
            next_sec += 1_000_000;
        }
        thread::sleep(Duration::from_micros(150));
    }
    ClientReport { frames_ok: ok, frames_bad: bad, audio, inputs_sent: inputs, latencies_us: lat, per_second, stats: p.ep.stats(), rejected: p.forged_or_rejected }
}

fn main() {
    let args = Arc::new(parse_args());

    // ── Host : identité éphémère + invitation à usage unique ──
    let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = host_sock.local_addr().unwrap();
    let mut auth = HostAuth::new(Identity::generate(), TrustStore::in_memory(), "loopback-host");
    let token = auth.trust.create_invite(60_000, now_ms()).unwrap();
    let invite = Invite { addr: addr.to_string(), host_key: auth.identity().public(), token: Some(token) };
    let uri = invite.to_uri();
    println!("[host] écoute sur {addr}\n[host] invitation : {uri}");

    // ── Client : n'a que le lien d'invitation ──
    let invite = Invite::parse(&uri).unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_id = Identity::generate();

    println!(
        "Flux : images de {} Ko à {} fps ({:.1} Mb/s de média), {} s, perte injectée {:.1}%\n",
        args.kb,
        args.fps,
        (args.kb * 1000 * 8) as f64 * args.fps as f64 / 1e6,
        args.secs,
        args.loss * 100.0
    );
    let t0 = Instant::now();
    let a1 = args.clone();
    let h = thread::spawn(move || run_host(host_sock, auth, &a1, t0));
    let a2 = args.clone();
    let c = thread::spawn(move || run_client(client_sock, invite, client_id, &a2, t0));
    let host = h.join().unwrap();
    let mut client = c.join().unwrap();

    client.latencies_us.sort_unstable();
    let pct = |q: usize| client.latencies_us.get(client.latencies_us.len().saturating_sub(1) * q / 100).copied().unwrap_or(0);
    println!("\n── Résumé ──");
    println!("images : {} émises, {} reçues intactes, {} corrompues", host.frames_sent, client.frames_ok, client.frames_bad);
    println!("latence image (capture->livraison, même horloge) : p50={} us  p99={} us  max={} us", pct(50), pct(99), pct(100));
    println!("audio : {} émis, {} reçus", host.audio_sent, client.audio);
    println!("entrées : {} envoyées, {} reçues dans l'ordre, {} hors-séquence", client.inputs_sent, host.input_ok, host.input_out_of_order);
    let vs = client.stats.video_rx;
    println!(
        "récepteur vidéo : livrées={} perdues={} récupérées par FEC={} IDR demandées={} paquets rejetés (chiffrement)={}",
        vs.frames_delivered, vs.frames_lost, vs.frames_recovered, vs.keyframe_requests, client.rejected
    );
    println!(
        "host : cible encodeur={:.1} Mb/s, FEC={}‰, perte vue par le client (rapport)={:.1}%",
        host.stats.target_bitrate_bps as f64 / 1e6,
        host.stats.fec_permille,
        host.stats.peer_loss * 100.0
    );
    println!(
        "host : {} paquets UDP, {:.1} Mb/s moyens sur le fil, retransmissions input={}",
        host.sent_pkts,
        host.stats.bytes_tx as f64 * 8.0 / (args.secs as f64 + 0.7) / 1e6,
        client.stats.input_retransmits
    );
    let _ = client.per_second;
    if client.frames_bad > 0 || host.input_out_of_order > 0 {
        std::process::exit(1);
    }
}
