//! `Endpoint` : un pair de transport média, SANS I/O.
//!
//! Il ne connaît ni socket ni chiffrement : il consomme et produit des *payloads applicatifs*
//! (le clair d'un `Session::seal` / `Session::open`) et lit l'heure qu'on lui donne
//! (`now`, microsecondes monotones). Tout est donc déterministe et testable.
//!
//! Boucle type :
//! ```text
//! recv(datagram) -> session.open() -> endpoint.handle_datagram(now, &payload)
//! while let Some(p) = endpoint.poll_transmit(now) { socket.send(session.seal(&p)) }
//! while let Some(ev) = endpoint.poll_event() { ... }
//! attendre jusqu'à endpoint.next_wakeup(now) ou le prochain datagramme
//! ```

use std::collections::VecDeque;

use crate::bitrate::{BitrateConfig, BitrateController, NetworkSample};
use crate::netstats::{MediaRx, RttEstimator};
use crate::pacer::Pacer;
use crate::reliable::{ReliableRx, ReliableTx};
use crate::video::{build_packets, FecConfig, VideoFrame, VideoReceiver, VideoRxConfig, VideoRxStats};
use crate::wire::*;
use crate::{Micros, SendError};

#[derive(Clone, Debug)]
pub struct Config {
    /// Taille maximale d'un payload applicatif (avant chiffrement). Session ajoute 29 octets.
    pub max_payload: usize,
    /// Accepte les paquets média (vidéo/audio) entrants. Le host le met à `false` : un client
    /// n'en envoie jamais, donc ils ne coûtent ni analyse ni mémoire (réassemblage vidéo).
    pub accept_media: bool,
    pub fec: FecConfig,
    /// Ajuste automatiquement le taux de FEC selon la perte mesurée par le pair.
    pub adaptive_fec: bool,
    pub video_rx: VideoRxConfig,
    /// Relance d'une demande d'image clé restée sans effet.
    pub keyframe_retry_us: Micros,
    pub pacing: bool,
    /// Débit de pacing = `pacing_factor` x budget total du contrôleur.
    pub pacing_factor: f64,
    pub pacing_burst_bytes: usize,
    pub pacing_min_bps: u64,
    /// File vidéo d'émission : au-delà de ce délai estimé, les images en attente sont jetées.
    pub max_video_queue_us: Micros,
    pub ping_interval_us: Micros,
    pub report_interval_us: Micros,
    /// Sans rapport du pair pendant ce délai alors qu'on envoie de la vidéo : on suppose
    /// une congestion sévère.
    pub report_timeout_us: Micros,
    pub bitrate: BitrateConfig,
    pub reliable_window: usize,
    pub reliable_backlog: usize,
    pub min_rto_us: Micros,
    pub max_rto_us: Micros,
    pub initial_rto_us: Micros,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_payload: 1200,
            accept_media: true,
            fec: FecConfig::default(),
            adaptive_fec: true,
            video_rx: VideoRxConfig::default(),
            keyframe_retry_us: 500_000,
            pacing: true,
            pacing_factor: 2.5,
            pacing_burst_bytes: 4 * 1300,
            pacing_min_bps: 1_000_000,
            max_video_queue_us: 150_000,
            ping_interval_us: 500_000,
            report_interval_us: 100_000,
            report_timeout_us: 1_000_000,
            bitrate: BitrateConfig::default(),
            reliable_window: 1024,
            reliable_backlog: 4096,
            min_rto_us: 20_000,
            max_rto_us: 1_000_000,
            initial_rto_us: 150_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPacket {
    /// Numéro de séquence audio (u16, rebouclant) : un trou = paquets perdus (PLC).
    pub seq: u16,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

/// Ce que l'application reçoit du transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Image vidéo complète et vérifiée, dans l'ordre, jamais périmée.
    Video(VideoFrame),
    Audio(AudioPacket),
    /// Message du canal Input (fiable, ordonné, sans doublon).
    Input(Vec<u8>),
    /// Message `Custom` du canal Control (fiable, ordonné).
    Control(Vec<u8>),
    /// L'encodeur doit produire une image clé (demande du pair, ou images jetées localement).
    KeyframeRequested,
    /// Datagramme applicatif NON fiable (ex. position du curseur) : peut être perdu, dupliqué ou
    /// réordonné ; à l'application de dater/numéroter son contenu.
    Datagram(Vec<u8>),
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub rtt_us: Option<Micros>,
    pub min_rtt_us: Option<Micros>,
    /// Perte mesurée localement sur le dernier intervalle (paquets média reçus).
    pub rx_loss: f32,
    /// Débit média reçu localement (bits/s, en-têtes inclus), dernier intervalle.
    pub rx_rate_bps: u64,
    /// Dernier rapport reçu du pair : perte de nos paquets média, débit qu'il reçoit.
    pub peer_loss: f32,
    pub peer_rx_rate_bps: u64,
    /// Bitrate à donner à l'encodeur / budget total / FEC / pacing courants.
    pub target_bitrate_bps: u64,
    pub total_bitrate_bps: u64,
    pub fec_permille: u16,
    pub pacing_rate_bps: u64,
    pub packets_tx: u64,
    pub bytes_tx: u64,
    pub packets_rx: u64,
    pub bytes_rx: u64,
    pub packets_invalid: u64,
    pub frames_sent: u64,
    /// Images jetées côté émetteur car la file de pacing était trop longue.
    pub frames_dropped_tx: u64,
    pub input_retransmits: u64,
    pub control_retransmits: u64,
    pub video_rx: VideoRxStats,
}

/// Taille maximale de `other_q`.
pub const MAX_OTHER_Q: usize = 256;

struct Queued {
    bytes: Vec<u8>,
    media: bool,
    frame: u32,
}

pub struct Endpoint {
    cfg: Config,
    fec: FecConfig,

    next_frame_id: u32,
    next_aseq: u16,
    media_tseq: u16,
    video_q: VecDeque<Queued>,
    video_q_bytes: usize,
    other_q: VecDeque<Queued>,
    rel_tx: [ReliableTx; 2],
    rel_rx: [ReliableRx; 2],
    ack_pending: [bool; 2],
    pacer: Pacer,

    video_rx: VideoReceiver,
    audio_last: Option<u16>,
    media_rx: MediaRx,

    rtt: RttEstimator,
    ctrl: BitrateController,
    events: VecDeque<Event>,
    next_ping: Option<Micros>,
    next_report: Option<Micros>,
    last_report_time: Micros,
    last_report_rx: Option<Micros>,
    last_video_tx: Option<Micros>,
    stats: Stats,
}

impl Endpoint {
    pub fn new(cfg: Config) -> Self {
        let ctrl = BitrateController::new(cfg.bitrate.clone());
        let pacer = Pacer::new(
            (ctrl.total_bps() as f64 * cfg.pacing_factor) as u64,
            cfg.pacing_burst_bytes.max(cfg.max_payload + 100),
        );
        Self {
            fec: cfg.fec,
            next_frame_id: 0,
            next_aseq: 0,
            media_tseq: 0,
            video_q: VecDeque::new(),
            video_q_bytes: 0,
            other_q: VecDeque::new(),
            rel_tx: [
                ReliableTx::new(0, cfg.reliable_window, cfg.reliable_backlog),
                ReliableTx::new(1, cfg.reliable_window, cfg.reliable_backlog),
            ],
            rel_rx: [ReliableRx::new(cfg.reliable_window), ReliableRx::new(cfg.reliable_window)],
            ack_pending: [false; 2],
            pacer,
            video_rx: VideoReceiver::new(cfg.video_rx),
            audio_last: None,
            media_rx: MediaRx::new(),
            rtt: RttEstimator::new(),
            ctrl,
            events: VecDeque::new(),
            next_ping: None,
            next_report: None,
            last_report_time: 0,
            last_report_rx: None,
            last_video_tx: None,
            stats: Stats::default(),
            cfg,
        }
    }

    // ───────────────────────────── émission ─────────────────────────────

    /// Envoie une image encodée (non fiable, FEC, pacing). Retourne son identifiant.
    pub fn send_video_frame(
        &mut self,
        now: Micros,
        data: &[u8],
        keyframe: bool,
        pts_us: u64,
    ) -> Result<u32, SendError> {
        let id = self.next_frame_id;
        let pkts = build_packets(&self.fec, self.cfg.max_payload, id, keyframe, pts_us, data)?;
        self.next_frame_id = id.wrapping_add(1);
        self.update_pacer(now);

        // Priorité à la latence : si la file est déjà trop longue, on jette tout le retard.
        if !self.video_q.is_empty() {
            let delay_us = self.video_q_bytes as f64 * 8e6 / self.pacer.rate_bps() as f64;
            if delay_us > self.cfg.max_video_queue_us as f64 {
                let mut frames = 0u64;
                let mut last = None;
                for q in &self.video_q {
                    if last != Some(q.frame) {
                        frames += 1;
                        last = Some(q.frame);
                    }
                }
                self.video_q.clear();
                self.video_q_bytes = 0;
                self.stats.frames_dropped_tx += frames;
                if !keyframe {
                    // Les références de cette image delta viennent d'être jetées.
                    self.events.push_back(Event::KeyframeRequested);
                }
            }
        }
        for p in pkts {
            self.video_q_bytes += p.len();
            self.video_q.push_back(Queued { bytes: p, media: true, frame: id });
        }
        self.stats.frames_sent += 1;
        self.last_video_tx = Some(now);
        if self.last_report_rx.is_none() {
            self.last_report_rx = Some(now);
        }
        Ok(id)
    }

    /// Paquet audio (non fiable, non paced).
    pub fn send_audio(&mut self, pts_us: u64, data: &[u8]) -> Result<(), SendError> {
        if data.is_empty() {
            return Err(SendError::Empty);
        }
        if AUDIO_HDR + data.len() > self.cfg.max_payload {
            return Err(SendError::TooLarge);
        }
        let mut p = Vec::with_capacity(AUDIO_HDR + data.len());
        p.push(b0(K_AUDIO, 0));
        p.extend_from_slice(&[0, 0]); // tseq : renseigné à l'émission
        p.extend_from_slice(&self.next_aseq.to_le_bytes());
        p.extend_from_slice(&pts_us.to_le_bytes());
        p.extend_from_slice(data);
        self.next_aseq = self.next_aseq.wrapping_add(1);
        self.push_other(Queued { bytes: p, media: true, frame: 0 });
        Ok(())
    }

    /// Événement d'entrée (clavier/souris/manette) : fiable, ordonné, retransmis.
    pub fn send_input(&mut self, data: &[u8]) -> Result<(), SendError> {
        self.send_reliable(0, data)
    }

    /// Message applicatif de contrôle : fiable, ordonné, retransmis.
    pub fn send_control(&mut self, data: &[u8]) -> Result<(), SendError> {
        self.send_reliable(1, &ControlMsg::Custom(data.to_vec()).encode())
    }

    /// Datagramme applicatif non fiable, non retransmis, non paced (petits messages fréquents
    /// « dernier état gagnant », ex. position du curseur). File bornée ([`MAX_OTHER_Q`]).
    pub fn send_datagram(&mut self, data: &[u8]) -> Result<(), SendError> {
        if data.is_empty() {
            return Err(SendError::Empty);
        }
        if 1 + data.len() > self.cfg.max_payload {
            return Err(SendError::TooLarge);
        }
        let mut p = Vec::with_capacity(1 + data.len());
        p.push(b0(K_RELIABLE, F_DATAGRAM));
        p.extend_from_slice(data);
        self.push_other(Queued { bytes: p, media: false, frame: 0 });
        Ok(())
    }

    /// Demande au pair une image clé (canal Control fiable).
    pub fn request_keyframe(&mut self) -> Result<(), SendError> {
        self.send_reliable(1, &ControlMsg::KeyframeRequest.encode())
    }

    fn send_reliable(&mut self, idx: usize, msg: &[u8]) -> Result<(), SendError> {
        if msg.is_empty() {
            return Err(SendError::Empty);
        }
        if RELIABLE_HDR + msg.len() > self.cfg.max_payload {
            return Err(SendError::TooLarge);
        }
        if self.rel_tx[idx].push(msg) {
            Ok(())
        } else {
            Err(SendError::Backlog)
        }
    }

    // ───────────────────────────── réception ─────────────────────────────

    /// Traite un payload reçu (déjà déchiffré et authentifié). Ne panique jamais, quel que
    /// soit le contenu.
    pub fn handle_datagram(&mut self, now: Micros, p: &[u8]) {
        self.stats.packets_rx += 1;
        self.stats.bytes_rx += p.len() as u64;
        let ok = match p.first().map(|&b| kind_of(b)) {
            Some(K_VIDEO) => self.cfg.accept_media && self.on_video(now, p),
            Some(K_AUDIO) => self.cfg.accept_media && self.on_audio(p),
            Some(K_PATH_CHALLENGE) => self.on_path_challenge(p),
            Some(K_RELIABLE) => self.on_reliable(p),
            Some(K_ACK) => self.on_ack(now, p),
            Some(K_PING) => self.on_ping(now, p),
            Some(K_REPORT) => self.on_report(now, p),
            _ => false,
        };
        if !ok {
            self.stats.packets_invalid += 1;
        }
        self.drain_video();
    }

    fn on_video(&mut self, now: Micros, p: &[u8]) -> bool {
        let Some((h, body)) = VideoHdr::parse(p) else { return false };
        self.media_rx.on_packet(h.tseq, p.len());
        self.video_rx.on_packet(now, &h, body);
        true
    }

    fn on_audio(&mut self, p: &[u8]) -> bool {
        if p.len() <= AUDIO_HDR {
            return false;
        }
        let tseq = u16::from_le_bytes([p[1], p[2]]);
        let seq = u16::from_le_bytes([p[3], p[4]]);
        let pts_us = u64::from_le_bytes(p[5..13].try_into().unwrap());
        self.media_rx.on_packet(tseq, p.len());
        if self.audio_last.is_some_and(|l| !seq16_lt(l, seq)) {
            return true; // doublon ou paquet en retard : le jouer serait pire que le perdre
        }
        self.audio_last = Some(seq);
        self.events.push_back(Event::Audio(AudioPacket { seq, pts_us, data: p[AUDIO_HDR..].to_vec() }));
        true
    }

    fn on_reliable(&mut self, p: &[u8]) -> bool {
        if flags_of(p[0]) & F_DATAGRAM != 0 {
            if p.len() < 2 {
                return false;
            }
            self.events.push_back(Event::Datagram(p[1..].to_vec()));
            return true;
        }
        if p.len() <= RELIABLE_HDR {
            return false;
        }
        let idx = (flags_of(p[0]) & 1) as usize;
        let seq = u16::from_le_bytes([p[1], p[2]]);
        let events = &mut self.events;
        self.rel_rx[idx].on_message(seq, &p[RELIABLE_HDR..], |m| {
            if idx == 0 {
                events.push_back(Event::Input(m));
            } else {
                match ControlMsg::decode(&m) {
                    Some(ControlMsg::KeyframeRequest) => events.push_back(Event::KeyframeRequested),
                    Some(ControlMsg::Custom(b)) => events.push_back(Event::Control(b)),
                    None => {}
                }
            }
        });
        self.ack_pending[idx] = true;
        true
    }

    fn on_ack(&mut self, now: Micros, p: &[u8]) -> bool {
        if p.len() < ACK_LEN {
            return false;
        }
        let idx = (flags_of(p[0]) & 1) as usize;
        let cum = u16::from_le_bytes([p[1], p[2]]);
        let sack = u64::from_le_bytes(p[3..11].try_into().unwrap());
        let gap = self.rtt.srtt().map_or(0, |s| s / 2);
        let rtt = &mut self.rtt;
        self.rel_tx[idx].on_ack(now, cum, sack, gap, |s| rtt.on_sample(now, s));
        true
    }

    /// Challenge de validation de chemin : on renvoie les 8 octets tels quels (depuis l'adresse
    /// où le pair nous voit maintenant, puisque la réponse suit le chemin sortant courant).
    fn on_path_challenge(&mut self, p: &[u8]) -> bool {
        if p.len() != PATH_LEN {
            return false;
        }
        let mut v = p.to_vec();
        v[0] = b0(K_PATH_RESPONSE, 0);
        self.push_other(Queued { bytes: v, media: false, frame: 0 });
        true
    }

    fn on_ping(&mut self, now: Micros, p: &[u8]) -> bool {
        if p.len() < PING_LEN {
            return false;
        }
        let ts = u64::from_le_bytes(p[1..9].try_into().unwrap());
        if flags_of(p[0]) & 1 == 0 {
            let mut v = Vec::with_capacity(PING_LEN);
            v.push(b0(K_PING, 1));
            v.extend_from_slice(&ts.to_le_bytes());
            self.push_other(Queued { bytes: v, media: false, frame: 0 });
        } else if now >= ts && now - ts < 30_000_000 {
            self.rtt.on_sample(now, now - ts);
        }
        true
    }

    fn on_report(&mut self, now: Micros, p: &[u8]) -> bool {
        let Some(r) = Report::parse(p) else { return false };
        self.last_report_rx = Some(now);
        self.stats.peer_loss = r.loss();
        self.stats.peer_rx_rate_bps = r.recv_kbps as u64 * 1000;
        self.ctrl.on_sample(
            now,
            NetworkSample {
                loss: r.loss(),
                rtt_us: self.rtt.srtt(),
                min_rtt_us: self.rtt.min_rtt(),
                recv_rate_bps: r.recv_kbps as u64 * 1000,
            },
        );
        if self.cfg.adaptive_fec {
            self.fec.parity_permille = self.ctrl.fec_permille();
        }
        true
    }

    /// File des petits paquets (ping/pong, rapports, audio, validation de chemin) bornée : un pair
    /// qui inonde de PING ne peut pas faire grossir la mémoire ni la file d'émission. Au-delà de
    /// la borne on jette le plus ancien.
    fn push_other(&mut self, q: Queued) {
        if self.other_q.len() >= MAX_OTHER_Q {
            self.other_q.pop_front();
        }
        self.other_q.push_back(q);
    }

    fn drain_video(&mut self) {
        while let Some(f) = self.video_rx.pop_frame() {
            self.events.push_back(Event::Video(f));
        }
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ───────────────────────────── horloge ─────────────────────────────

    fn rto(&self) -> Micros {
        self.rtt.rto(self.cfg.min_rto_us, self.cfg.max_rto_us, self.cfg.initial_rto_us)
    }

    fn update_pacer(&mut self, now: Micros) {
        let rate = (self.ctrl.total_bps() as f64 * self.cfg.pacing_factor) as u64;
        self.pacer.set_rate(now, rate.max(self.cfg.pacing_min_bps));
    }

    /// Timers internes : livraison/abandon des images, demandes d'image clé, ping, rapports.
    pub fn tick(&mut self, now: Micros) {
        self.video_rx.process(now);
        self.drain_video();

        let srtt = self.rtt.srtt().unwrap_or(0);
        let min_interval = (2 * srtt).max(100_000);
        let retry = (4 * srtt).max(self.cfg.keyframe_retry_us);
        if self.video_rx.keyframe_due(now, min_interval, retry) {
            // Si la file de contrôle est saturée, on réessaiera à la relance suivante.
            let _ = self.request_keyframe();
        }

        if now >= *self.next_ping.get_or_insert(now) {
            let mut v = Vec::with_capacity(PING_LEN);
            v.push(b0(K_PING, 0));
            v.extend_from_slice(&now.to_le_bytes());
            self.push_other(Queued { bytes: v, media: false, frame: 0 });
            self.next_ping = Some(now + self.cfg.ping_interval_us);
        }

        if self.next_report.is_none() {
            self.last_report_time = now;
        }
        let due = *self.next_report.get_or_insert(now + self.cfg.report_interval_us);
        if now >= due {
            self.next_report = Some(now + self.cfg.report_interval_us);
            if self.media_rx.has_data() {
                let (loss, bytes) = self.media_rx.close_interval();
                let elapsed = now.saturating_sub(self.last_report_time).max(1);
                let rate = bytes * 8_000_000 / elapsed;
                self.stats.rx_loss = loss;
                self.stats.rx_rate_bps = rate;
                let rep = Report {
                    loss_q8: (loss * 255.0).round() as u8,
                    recv_kbps: (rate / 1000).min(u32::MAX as u64) as u32,
                    frames_lost: self.video_rx.take_lost_counter().min(u16::MAX as u64) as u16,
                };
                self.push_other(Queued { bytes: rep.write(), media: false, frame: 0 });
            }
            self.last_report_time = now;
            // Rien reçu du pair alors qu'on émet : congestion sévère présumée.
            if self.last_video_tx.is_some_and(|t| now.saturating_sub(t) < self.cfg.report_timeout_us) {
                let silent = now.saturating_sub(self.last_report_rx.unwrap_or(now));
                if silent >= self.cfg.report_timeout_us {
                    self.ctrl.on_sample(now, NetworkSample { loss: 0.5, ..Default::default() });
                    self.last_report_rx = Some(now);
                }
            }
        }
    }

    /// Prochain paquet à émettre (payload à chiffrer), ou `None`.
    pub fn poll_transmit(&mut self, now: Micros) -> Option<Vec<u8>> {
        self.tick(now);
        for i in 0..2 {
            if self.ack_pending[i] {
                self.ack_pending[i] = false;
                return Some(self.finish_plain(self.rel_rx[i].build_ack(i as u8)));
            }
        }
        let rto = self.rto();
        for i in 0..2 {
            if let Some(p) = self.rel_tx[i].poll(now, rto) {
                return Some(self.finish_plain(p));
            }
        }
        if let Some(q) = self.other_q.pop_front() {
            return Some(self.finish(q));
        }
        if let Some(front) = self.video_q.front() {
            let len = front.bytes.len();
            if !self.cfg.pacing {
                return self.pop_video();
            }
            self.update_pacer(now);
            if self.pacer.try_send(now, len).is_ok() {
                return self.pop_video();
            }
        }
        None
    }

    fn pop_video(&mut self) -> Option<Vec<u8>> {
        let q = self.video_q.pop_front()?;
        self.video_q_bytes -= q.bytes.len();
        Some(self.finish(q))
    }

    fn finish_plain(&mut self, p: Vec<u8>) -> Vec<u8> {
        self.stats.packets_tx += 1;
        self.stats.bytes_tx += p.len() as u64;
        p
    }

    fn finish(&mut self, mut q: Queued) -> Vec<u8> {
        if q.media {
            q.bytes[1..3].copy_from_slice(&self.media_tseq.to_le_bytes());
            self.media_tseq = self.media_tseq.wrapping_add(1);
        }
        self.finish_plain(q.bytes)
    }

    /// Prochain instant où `tick`/`poll_transmit` auront du travail (None = rien de planifié).
    pub fn next_wakeup(&self, now: Micros) -> Option<Micros> {
        if self.ack_pending.iter().any(|&a| a) || !self.other_q.is_empty() {
            return Some(now);
        }
        let mut best: Option<Micros> = None;
        let mut add = |t: Micros| best = Some(best.map_or(t, |b| b.min(t)));
        let rto = self.rto();
        for r in &self.rel_tx {
            if let Some(t) = r.next_deadline(rto) {
                add(t.max(now));
            }
        }
        if let Some(front) = self.video_q.front() {
            add(if self.cfg.pacing { self.pacer.ready_at(now, front.bytes.len()) } else { now });
        }
        if let Some(t) = self.video_rx.next_deadline() {
            add(t.max(now));
        }
        if let Some(t) = self.next_ping {
            add(t.max(now));
        }
        if let Some(t) = self.next_report {
            add(t.max(now));
        }
        best
    }

    // ───────────────────────────── état ─────────────────────────────

    /// Bitrate cible à donner à l'encodeur (bits/s), sans la redondance FEC.
    pub fn target_bitrate_bps(&self) -> u64 {
        self.ctrl.encoder_bps()
    }

    pub fn rtt_us(&self) -> Option<Micros> {
        self.rtt.srtt()
    }

    pub fn bitrate_controller(&self) -> &BitrateController {
        &self.ctrl
    }

    /// Plafond à chaud du débit total (média + FEC), ex. « bande passante max » du client ;
    /// `None` rétablit `Config::bitrate.max_bps`. Voir [`BitrateController::set_cap`].
    pub fn set_max_bitrate_bps(&mut self, cap_bps: Option<u64>) {
        self.ctrl.set_cap(cap_bps);
    }

    /// Remplace la configuration FEC (à utiliser avec `adaptive_fec = false`).
    pub fn set_fec(&mut self, fec: FecConfig) {
        self.fec = fec;
    }

    pub fn fec(&self) -> FecConfig {
        self.fec
    }

    pub fn stats(&self) -> Stats {
        let mut s = self.stats.clone();
        s.rtt_us = self.rtt.srtt();
        s.min_rtt_us = self.rtt.min_rtt();
        s.target_bitrate_bps = self.ctrl.encoder_bps();
        s.total_bitrate_bps = self.ctrl.total_bps();
        s.fec_permille = self.fec.parity_permille;
        s.pacing_rate_bps = self.pacer.rate_bps();
        s.input_retransmits = self.rel_tx[0].retransmits;
        s.control_retransmits = self.rel_tx[1].retransmits;
        s.video_rx = self.video_rx.stats;
        s
    }

    /// Messages fiables émis mais pas encore acquittés (Input, Control).
    pub fn reliable_unacked(&self) -> (usize, usize) {
        (self.rel_tx[0].unacked(), self.rel_tx[1].unacked())
    }

    /// Octets vidéo en attente d'émission (pacing).
    pub fn video_queue_bytes(&self) -> usize {
        self.video_q_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_pkt() -> Vec<u8> {
        // En-tête vidéo valide (image de 3000 octets annoncés en 3 fragments de 1182) + corps.
        let h = VideoHdr {
            keyframe: true,
            parity: false,
            tseq: 0,
            frame_id: 7,
            frame_len: 30,
            shard_size: 1182,
            kmax: 10,
            m: 1,
            group: 0,
            idx: 0,
        };
        let mut v = Vec::new();
        h.write(&mut v);
        v.extend_from_slice(&[0u8; 30]);
        v
    }

    fn ping(ts: u64) -> Vec<u8> {
        let mut v = vec![b0(K_PING, 0)];
        v.extend_from_slice(&ts.to_le_bytes());
        v
    }

    #[test]
    fn host_side_endpoint_ignores_incoming_media() {
        let mut host = Endpoint::new(Config { accept_media: false, ..Default::default() });
        host.handle_datagram(0, &video_pkt());
        let mut audio = vec![b0(K_AUDIO, 0), 0, 0, 0, 0];
        audio.extend_from_slice(&0u64.to_le_bytes());
        audio.extend_from_slice(b"opus");
        host.handle_datagram(0, &audio);
        assert_eq!(host.stats().packets_invalid, 2);
        assert_eq!(host.stats().video_rx.frames_delivered, 0);
        assert!(host.poll_event().is_none(), "aucun événement audio/vidéo");

        // Le comportement par défaut (client) reste inchangé : l'audio est livré.
        let mut client = Endpoint::new(Config::default());
        client.handle_datagram(0, &video_pkt());
        client.handle_datagram(0, &audio);
        assert!(matches!(client.poll_event(), Some(Event::Audio(_))));
        assert_eq!(client.stats().packets_invalid, 0);
    }

    #[test]
    fn small_packet_queue_is_bounded() {
        let mut ep = Endpoint::new(Config::default());
        for i in 0..10_000u64 {
            ep.handle_datagram(i, &ping(i)); // chaque PING appelle un PONG
        }
        let mut n = 0;
        while ep.poll_transmit(10_000).is_some() {
            n += 1;
            assert!(n < 10_000, "la file de sortie ne se vide pas");
        }
        // 256 pongs au plus (+ quelques paquets internes : ping, ack éventuel).
        assert!(n <= MAX_OTHER_Q + 4, "{n} paquets émis");
        assert!(ep.other_q.len() <= MAX_OTHER_Q);
    }

    #[test]
    fn datagrams_are_delivered_unreliably_and_bounded() {
        let mut a = Endpoint::new(Config::default());
        let mut b = Endpoint::new(Config::default());
        assert_eq!(a.send_datagram(&[]), Err(SendError::Empty));
        assert_eq!(a.send_datagram(&[0u8; 1200]), Err(SendError::TooLarge));
        a.send_datagram(b"pos1").unwrap();
        a.send_datagram(b"pos2").unwrap();
        while let Some(p) = a.poll_transmit(0) {
            b.handle_datagram(0, &p);
        }
        let got: Vec<_> = std::iter::from_fn(|| b.poll_event()).filter_map(|e| match e {
            Event::Datagram(d) => Some(d),
            _ => None,
        }).collect();
        assert_eq!(got, vec![b"pos1".to_vec(), b"pos2".to_vec()]);
        // Aucun ACK, aucune retransmission.
        assert_eq!(b.reliable_unacked(), (0, 0));
        assert!(b.poll_transmit(0).map_or(true, |p| kind_of(p[0]) != K_ACK));
        // Datagramme vide : invalide.
        b.handle_datagram(0, &[b0(K_RELIABLE, F_DATAGRAM)]);
        assert_eq!(b.stats().packets_invalid, 1);
        // File bornée.
        for _ in 0..10_000 {
            a.send_datagram(b"x").unwrap();
        }
        assert!(a.other_q.len() <= MAX_OTHER_Q);
    }

    #[test]
    fn path_challenge_is_echoed_as_response() {
        let mut ep = Endpoint::new(Config::default());
        let tok = [1, 2, 3, 4, 5, 6, 7, 8];
        ep.handle_datagram(0, &path_challenge(tok));
        let mut found = None;
        while let Some(p) = ep.poll_transmit(0) {
            if let Some(t) = parse_path_response(&p) {
                found = Some(t);
            }
        }
        assert_eq!(found, Some(tok));
        // Un challenge mal formé est invalide, pas répété.
        ep.handle_datagram(0, &[b0(K_PATH_CHALLENGE, 0), 1, 2]);
        assert_eq!(ep.stats().packets_invalid, 1);
    }
}
