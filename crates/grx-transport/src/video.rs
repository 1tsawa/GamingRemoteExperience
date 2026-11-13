//! Vidéo : fragmentation + FEC Reed-Solomon côté émetteur, réassemblage + politique de
//! livraison (latence minimale, jamais d'image incomplète) côté récepteur.

use std::collections::VecDeque;

use crate::fec;
use crate::wire::{seq32_lt, VideoHdr, VIDEO_HDR};
use crate::{Micros, SendError};

/// Préambule ajouté à chaque image avant fragmentation : `crc32 u32 | pts_us u64`.
pub const PRELUDE: usize = 12;
/// Taille maximale d'une image (préambule inclus).
pub const MAX_FRAME_BYTES: usize = 32 << 20;
const MAX_SHARDS_TOTAL: usize = 40_000;
const MAX_GROUP_DATA: usize = 200;

/// Paramètres FEC (côté émetteur uniquement : le récepteur lit la structure dans les en-têtes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FecConfig {
    /// Nombre maximal de fragments de données par groupe RS (1..=200).
    pub group_data: usize,
    /// Redondance en pour-mille des fragments de données d'un groupe (120 = 12 %).
    pub parity_permille: u16,
    /// Nombre minimal de fragments de parité par groupe (0 = aucun si permille == 0).
    pub min_parity: usize,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self { group_data: 100, parity_permille: 120, min_parity: 1 }
    }
}

impl FecConfig {
    pub fn disabled() -> Self {
        Self { group_data: 100, parity_permille: 0, min_parity: 0 }
    }
}

/// Découpage d'une image en groupes RS. Entièrement déterminé par les champs d'en-tête.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub frame_len: usize,
    pub shard_size: usize,
    pub m: usize,
    pub n_data: usize,
    pub groups: usize,
}

impl Layout {
    pub fn new(frame_len: usize, shard_size: usize, kmax: usize, m: usize) -> Option<Layout> {
        if frame_len == 0 || frame_len > MAX_FRAME_BYTES || shard_size == 0 || kmax == 0 {
            return None;
        }
        let n_data = frame_len.div_ceil(shard_size);
        let groups = n_data.div_ceil(kmax);
        let k_big = n_data.div_ceil(groups);
        if k_big + m > 255 || n_data + groups * m > MAX_SHARDS_TOTAL || groups > 65_535 {
            return None;
        }
        Some(Layout { frame_len, shard_size, m, n_data, groups })
    }
    pub fn k(&self, g: usize) -> usize {
        self.n_data / self.groups + usize::from(g < self.n_data % self.groups)
    }
    fn data_before(&self, g: usize) -> usize {
        g * (self.n_data / self.groups) + g.min(self.n_data % self.groups)
    }
    /// Début du groupe `g` dans le tableau plat (données puis parité, groupe après groupe).
    pub fn group_start(&self, g: usize) -> usize {
        self.data_before(g) + g * self.m
    }
    pub fn total(&self) -> usize {
        self.n_data + self.groups * self.m
    }
    /// Longueur utile (sans bourrage) du fragment de données `pos` du groupe `g`.
    pub fn data_len(&self, g: usize, pos: usize) -> usize {
        let s = self.data_before(g) + pos;
        self.shard_size.min(self.frame_len - s * self.shard_size)
    }
}

/// Fragmente une image : retourne les paquets (payload applicatif, `tseq` = 0 à renseigner
/// à l'émission) dans l'ordre d'envoi (groupes entrelacés, parité en fin).
pub fn build_packets(
    fec_cfg: &FecConfig,
    max_payload: usize,
    frame_id: u32,
    keyframe: bool,
    pts_us: u64,
    data: &[u8],
) -> Result<Vec<Vec<u8>>, SendError> {
    if data.is_empty() {
        return Err(SendError::Empty);
    }
    let shard_size = max_payload.saturating_sub(VIDEO_HDR).min(u16::MAX as usize);
    if shard_size < 64 {
        return Err(SendError::InvalidConfig);
    }
    let total_len = PRELUDE + data.len();
    if total_len > MAX_FRAME_BYTES {
        return Err(SendError::TooLarge);
    }
    let kmax = fec_cfg.group_data.clamp(1, MAX_GROUP_DATA);
    let n_data = total_len.div_ceil(shard_size);
    let groups = n_data.div_ceil(kmax);
    let k_big = n_data.div_ceil(groups);
    let m = if fec_cfg.parity_permille == 0 && fec_cfg.min_parity == 0 {
        0
    } else {
        (k_big * fec_cfg.parity_permille as usize).div_ceil(1000).max(fec_cfg.min_parity)
    }
    .min(255 - k_big);
    let lay = Layout::new(total_len, shard_size, kmax, m).ok_or(SendError::TooLarge)?;

    let mut buf = vec![0u8; lay.n_data * shard_size];
    buf[4..12].copy_from_slice(&pts_us.to_le_bytes());
    buf[PRELUDE..total_len].copy_from_slice(data);
    let crc = fec::crc32(&buf[4..total_len]);
    buf[0..4].copy_from_slice(&crc.to_le_bytes());

    let mut parity: Vec<Vec<Vec<u8>>> = Vec::with_capacity(lay.groups);
    for g in 0..lay.groups {
        if m == 0 {
            parity.push(Vec::new());
            continue;
        }
        let first = lay.group_start(g) - g * m; // index global de la 1re donnée du groupe
        let refs: Vec<&[u8]> =
            (0..lay.k(g)).map(|p| &buf[(first + p) * shard_size..(first + p + 1) * shard_size]).collect();
        parity.push(fec::encode(&refs, m));
    }

    let mut hdr = VideoHdr {
        keyframe,
        parity: false,
        tseq: 0,
        frame_id,
        frame_len: total_len as u32,
        shard_size: shard_size as u16,
        kmax: kmax as u8,
        m: m as u8,
        group: 0,
        idx: 0,
    };
    let max_len = (0..lay.groups).map(|g| lay.k(g) + m).max().unwrap_or(0);
    let mut out = Vec::with_capacity(lay.total());
    for i in 0..max_len {
        for g in 0..lay.groups {
            let k = lay.k(g);
            if i >= k + m {
                continue;
            }
            hdr.group = g as u16;
            hdr.idx = i as u8;
            let mut pkt = Vec::with_capacity(VIDEO_HDR + shard_size);
            if i < k {
                hdr.parity = false;
                hdr.write(&mut pkt);
                let first = lay.group_start(g) - g * m;
                let s = first + i;
                pkt.extend_from_slice(&buf[s * shard_size..s * shard_size + lay.data_len(g, i)]);
            } else {
                hdr.parity = true;
                hdr.write(&mut pkt);
                pkt.extend_from_slice(&parity[g][i - k]);
            }
            out.push(pkt);
        }
    }
    Ok(out)
}

// ───────────────────────────── réception ─────────────────────────────

/// Image vidéo complète et vérifiée (CRC) livrée au décodeur.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub id: u32,
    pub keyframe: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoRxConfig {
    /// Une image dont le 1er paquet date de plus de ce délai est abandonnée.
    pub frame_timeout_us: Micros,
    /// Si une image plus récente est complète, on n'attend plus l'ancienne que ce délai
    /// après son dernier paquet reçu (tolérance au réordonnancement).
    pub reorder_grace_us: Micros,
    pub max_pending: usize,
    /// Après une perte, ne livrer aucune image delta jusqu'à la prochaine image clé
    /// (le décodeur n'aurait pas ses références : image corrompue à l'écran).
    pub drop_deltas_after_loss: bool,
}

impl Default for VideoRxConfig {
    fn default() -> Self {
        Self { frame_timeout_us: 100_000, reorder_grace_us: 15_000, max_pending: 8, drop_deltas_after_loss: true }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VideoRxStats {
    pub frames_delivered: u64,
    /// Images abandonnées (jamais livrées car irrécupérables / périmées).
    pub frames_lost: u64,
    /// Images livrées grâce à la FEC (au moins un fragment reconstruit).
    pub frames_recovered: u64,
    pub frames_corrupt: u64,
    /// Images delta complètes mais écartées en attendant une image clé.
    pub deltas_dropped: u64,
    pub packets_stale: u64,
    pub packets_invalid: u64,
    pub keyframe_requests: u64,
}

enum State {
    Collecting,
    Ready(VideoFrame, bool),
    Failed,
}

struct Asm {
    id: u32,
    keyframe: bool,
    kmax: u8,
    lay: Layout,
    shards: Vec<Option<Vec<u8>>>,
    have: Vec<u16>,
    first_rx: Micros,
    last_rx: Micros,
    state: State,
}

impl Asm {
    fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready(..))
    }

    fn try_assemble(&mut self) {
        let lay = self.lay;
        if (0..lay.groups).any(|g| (self.have[g] as usize) < lay.k(g)) {
            return;
        }
        let mut recovered = false;
        for g in 0..lay.groups {
            let k = lay.k(g);
            let start = lay.group_start(g);
            let grp = &mut self.shards[start..start + k + lay.m];
            if grp[..k].iter().any(|s| s.is_none()) {
                recovered = true;
                if !fec::reconstruct(k, lay.m, grp) {
                    self.state = State::Failed;
                    return;
                }
            }
        }
        let mut out = Vec::with_capacity(lay.n_data * lay.shard_size);
        for g in 0..lay.groups {
            let start = lay.group_start(g);
            for p in 0..lay.k(g) {
                out.extend_from_slice(self.shards[start + p].as_ref().unwrap());
            }
        }
        out.truncate(lay.frame_len);
        self.shards = Vec::new();
        let crc = u32::from_le_bytes(out[0..4].try_into().unwrap());
        if crc != fec::crc32(&out[4..]) {
            self.state = State::Failed;
            return;
        }
        let pts = u64::from_le_bytes(out[4..12].try_into().unwrap());
        out.drain(..PRELUDE);
        self.state = State::Ready(VideoFrame { id: self.id, keyframe: self.keyframe, pts_us: pts, data: out }, recovered);
    }
}

pub struct VideoReceiver {
    cfg: VideoRxConfig,
    expected: Option<u32>,
    pending: Vec<Asm>,
    out: VecDeque<VideoFrame>,
    waiting_key: bool,
    key_req_pending: bool,
    last_req: Option<Micros>,
    pub stats: VideoRxStats,
    lost_counter: u64,
}

impl VideoReceiver {
    pub fn new(cfg: VideoRxConfig) -> Self {
        Self {
            cfg,
            expected: None,
            pending: Vec::new(),
            out: VecDeque::new(),
            waiting_key: true,
            key_req_pending: false,
            last_req: None,
            stats: VideoRxStats::default(),
            lost_counter: 0,
        }
    }

    pub fn pop_frame(&mut self) -> Option<VideoFrame> {
        self.out.pop_front()
    }

    /// Nombre d'images abandonnées depuis le dernier appel (pour le rapport au pair).
    pub fn take_lost_counter(&mut self) -> u64 {
        std::mem::take(&mut self.lost_counter)
    }

    pub fn on_packet(&mut self, now: Micros, h: &VideoHdr, body: &[u8]) {
        if !self.accept(now, h, body) {
            self.stats.packets_invalid += 1;
        }
        self.process(now);
    }

    fn accept(&mut self, now: Micros, h: &VideoHdr, body: &[u8]) -> bool {
        if (h.frame_len as usize) <= PRELUDE {
            return false;
        }
        let Some(lay) = Layout::new(h.frame_len as usize, h.shard_size as usize, h.kmax as usize, h.m as usize)
        else {
            return false;
        };
        let g = h.group as usize;
        if g >= lay.groups {
            return false;
        }
        let k = lay.k(g);
        let idx = h.idx as usize;
        let pos_ok = if h.parity {
            idx >= k && idx < k + lay.m && body.len() == lay.shard_size
        } else {
            idx < k && body.len() == lay.data_len(g, idx)
        };
        if !pos_ok {
            return false;
        }
        if let Some(e) = self.expected {
            if seq32_lt(h.frame_id, e) {
                self.stats.packets_stale += 1;
                return true;
            }
            if h.frame_id.wrapping_sub(e) > 1024 {
                // Le flux a sauté loin devant (longue coupure) : on se resynchronise.
                self.pending.clear();
                self.expected = None;
                self.waiting_key = self.cfg.drop_deltas_after_loss;
            }
        }
        let i = match self.pending.iter().position(|a| a.id == h.frame_id) {
            Some(i) => i,
            None => {
                self.pending.push(Asm {
                    id: h.frame_id,
                    keyframe: h.keyframe,
                    kmax: h.kmax,
                    lay,
                    shards: vec![None; lay.total()],
                    have: vec![0; lay.groups],
                    first_rx: now,
                    last_rx: now,
                    state: State::Collecting,
                });
                self.pending.len() - 1
            }
        };
        let a = &mut self.pending[i];
        if a.lay != lay || a.keyframe != h.keyframe || a.kmax != h.kmax {
            return false;
        }
        a.last_rx = now;
        if !matches!(a.state, State::Collecting) {
            return true; // doublon
        }
        let slot = lay.group_start(g) + idx;
        if a.shards[slot].is_some() {
            return true; // doublon
        }
        let mut v = Vec::with_capacity(lay.shard_size);
        v.extend_from_slice(body);
        v.resize(lay.shard_size, 0);
        a.shards[slot] = Some(v);
        a.have[g] += 1;
        a.try_assemble();
        true
    }

    fn abandon(&mut self) {
        self.stats.frames_lost += 1;
        self.lost_counter += 1;
        if self.cfg.drop_deltas_after_loss {
            self.waiting_key = true;
        }
        self.key_req_pending = true;
    }

    fn deliver(&mut self, f: VideoFrame, recovered: bool) {
        if f.keyframe {
            self.waiting_key = false;
            self.key_req_pending = false;
        }
        if recovered {
            self.stats.frames_recovered += 1;
        }
        self.stats.frames_delivered += 1;
        self.out.push_back(f);
    }

    /// Fait avancer la machine à états de livraison (à appeler régulièrement).
    pub fn process(&mut self, now: Micros) {
        loop {
            if self.expected.is_none() {
                let oldest = self.pending.iter().map(|a| a.id).reduce(|a, b| if seq32_lt(b, a) { b } else { a });
                match oldest {
                    Some(id) => self.expected = Some(id),
                    None => return,
                }
            }
            let e = self.expected.unwrap();
            self.pending.retain(|a| !seq32_lt(a.id, e));

            // 1. Une image clé complète et plus récente que l'attendue : on saute dessus,
            //    tout ce qui la précède est périmé.
            let kf = self
                .pending
                .iter()
                .enumerate()
                .filter(|(_, a)| a.is_ready() && a.keyframe && a.id != e)
                .max_by_key(|(_, a)| a.id.wrapping_sub(e))
                .map(|(i, _)| i);
            if let Some(i) = kf {
                let a = self.pending.remove(i);
                let skipped = a.id.wrapping_sub(e) as u64;
                self.stats.frames_lost += skipped;
                self.lost_counter += skipped;
                let id = a.id;
                self.pending.retain(|p| seq32_lt(id, p.id));
                if let State::Ready(f, rec) = a.state {
                    self.deliver(f, rec);
                }
                self.expected = Some(id.wrapping_add(1));
                continue;
            }

            // 2. L'image attendue est-elle résolue ?
            if let Some(i) = self.pending.iter().position(|a| a.id == e) {
                match self.pending[i].state {
                    State::Ready(..) => {
                        let a = self.pending.remove(i);
                        if let State::Ready(f, rec) = a.state {
                            if self.waiting_key && !f.keyframe {
                                self.stats.deltas_dropped += 1;
                                self.key_req_pending = true;
                            } else {
                                self.deliver(f, rec);
                            }
                        }
                        self.expected = Some(e.wrapping_add(1));
                        continue;
                    }
                    State::Failed => {
                        self.pending.remove(i);
                        self.stats.frames_corrupt += 1;
                        self.abandon();
                        self.expected = Some(e.wrapping_add(1));
                        continue;
                    }
                    State::Collecting => {}
                }
            }

            // 3. Faut-il renoncer à l'image attendue ?
            let found = self.pending.iter().find(|a| a.id == e).map(|a| (a.first_rx, a.last_rx));
            let (first, last) = match found {
                Some(t) => t,
                None => match self.pending.iter().map(|a| a.first_rx).min() {
                    Some(f) => (f, f),
                    None => return,
                },
            };
            let newer_ready = self.pending.iter().any(|a| a.is_ready());
            let timeout = now.saturating_sub(first) >= self.cfg.frame_timeout_us;
            let grace = newer_ready && now.saturating_sub(last) >= self.cfg.reorder_grace_us;
            let overflow = self.pending.len() > self.cfg.max_pending;
            if timeout || grace || overflow {
                self.pending.retain(|a| a.id != e);
                self.abandon();
                self.expected = Some(e.wrapping_add(1));
                continue;
            }
            return;
        }
    }

    /// Faut-il envoyer (maintenant) une demande d'image clé ?
    /// `min_interval` : délai minimal entre deux demandes ; `retry` : relance si rien reçu.
    pub fn keyframe_due(&mut self, now: Micros, min_interval: Micros, retry: Micros) -> bool {
        let since = self.last_req.map(|t| now.saturating_sub(t));
        let fire = if self.key_req_pending {
            since.is_none_or(|s| s >= min_interval)
        } else if self.waiting_key {
            since.is_some_and(|s| s >= retry)
        } else {
            false
        };
        if fire {
            self.last_req = Some(now);
            self.key_req_pending = false;
            self.stats.keyframe_requests += 1;
        }
        fire
    }

    /// Prochaine échéance interne (abandon d'image), pour les boucles événementielles.
    pub fn next_deadline(&self) -> Option<Micros> {
        let mut best: Option<Micros> = None;
        let any_ready = self.pending.iter().any(|a| a.is_ready());
        for a in &self.pending {
            let mut t = a.first_rx + self.cfg.frame_timeout_us;
            if any_ready {
                t = t.min(a.last_rx + self.cfg.reorder_grace_us);
            }
            best = Some(best.map_or(t, |b| b.min(t)));
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn feed(rx: &mut VideoReceiver, now: Micros, pkts: &[Vec<u8>]) {
        for p in pkts {
            let (h, body) = VideoHdr::parse(p).unwrap();
            rx.on_packet(now, &h, body);
        }
    }

    #[test]
    fn packets_respect_mtu_and_roundtrip() {
        for &len in &[1usize, 100, 1171, 1172, 5000, 100_000, 400_000] {
            let data = frame(len, 3);
            let pk = build_packets(&FecConfig::default(), 1200, 7, true, 42, &data).unwrap();
            assert!(pk.iter().all(|p| p.len() <= 1200), "len {len}");
            let mut rx = VideoReceiver::new(VideoRxConfig::default());
            feed(&mut rx, 0, &pk);
            let f = rx.pop_frame().expect("frame");
            assert_eq!((f.id, f.keyframe, f.pts_us), (7, true, 42));
            assert_eq!(f.data, data);
            assert!(rx.pop_frame().is_none());
        }
    }

    #[test]
    fn fec_recovers_losses_up_to_parity() {
        let data = frame(100_000, 9);
        let pk = build_packets(&FecConfig { group_data: 200, parity_permille: 100, min_parity: 1 }, 1200, 1, true, 0, &data)
            .unwrap();
        let (h, _) = VideoHdr::parse(&pk[0]).unwrap();
        let m = h.m as usize;
        assert!(m >= 8);
        // On perd exactement m paquets (au début, donc des données).
        let mut rx = VideoReceiver::new(VideoRxConfig::default());
        feed(&mut rx, 0, &pk[m..]);
        assert_eq!(rx.pop_frame().unwrap().data, data);
        assert_eq!(rx.stats.frames_recovered, 1);
        // m + 1 pertes => rien n'est livré.
        let mut rx = VideoReceiver::new(VideoRxConfig::default());
        feed(&mut rx, 0, &pk[m + 1..]);
        assert!(rx.pop_frame().is_none());
    }

    #[test]
    fn incomplete_frame_is_never_delivered_and_requests_keyframe() {
        let cfg = FecConfig::disabled();
        let k = build_packets(&cfg, 1200, 0, true, 0, &frame(5000, 1)).unwrap();
        let d1 = build_packets(&cfg, 1200, 1, false, 0, &frame(5000, 2)).unwrap();
        let d2 = build_packets(&cfg, 1200, 2, false, 0, &frame(5000, 3)).unwrap();
        let mut rx = VideoReceiver::new(VideoRxConfig::default());
        feed(&mut rx, 0, &k);
        assert_eq!(rx.pop_frame().unwrap().id, 0);
        // d1 perd un paquet ; d2 complet arrive.
        feed(&mut rx, 10_000, &d1[1..]);
        feed(&mut rx, 20_000, &d2);
        assert!(rx.pop_frame().is_none(), "d2 attend d1 (grâce de réordonnancement)");
        rx.process(20_000 + 15_000);
        // d1 abandonnée ; d2 (delta) écartée car les références manquent.
        assert!(rx.pop_frame().is_none());
        assert_eq!(rx.stats.frames_lost, 1);
        assert_eq!(rx.stats.deltas_dropped, 1);
        assert!(rx.keyframe_due(40_000, 100_000, 500_000));
        assert!(!rx.keyframe_due(41_000, 100_000, 500_000));
        // La keyframe suivante repart proprement.
        let k2 = build_packets(&cfg, 1200, 3, true, 0, &frame(5000, 4)).unwrap();
        feed(&mut rx, 50_000, &k2);
        assert_eq!(rx.pop_frame().unwrap().id, 3);
        assert!(!rx.keyframe_due(1_000_000, 100_000, 500_000));
    }

    #[test]
    fn reordered_packets_and_duplicates_are_fine() {
        let data = frame(30_000, 5);
        let mut pk = build_packets(&FecConfig::default(), 1200, 0, true, 0, &data).unwrap();
        pk.reverse();
        let dup = pk.clone();
        let mut rx = VideoReceiver::new(VideoRxConfig::default());
        feed(&mut rx, 0, &pk);
        feed(&mut rx, 1, &dup);
        assert_eq!(rx.pop_frame().unwrap().data, data);
        assert!(rx.pop_frame().is_none());
    }

    #[test]
    fn newer_keyframe_discards_stale_frames() {
        let cfg = FecConfig::disabled();
        let k0 = build_packets(&cfg, 1200, 0, true, 0, &frame(3000, 1)).unwrap();
        let d1 = build_packets(&cfg, 1200, 1, false, 0, &frame(3000, 2)).unwrap();
        let k5 = build_packets(&cfg, 1200, 5, true, 0, &frame(3000, 3)).unwrap();
        let mut rx = VideoReceiver::new(VideoRxConfig::default());
        feed(&mut rx, 0, &k0);
        rx.pop_frame().unwrap();
        feed(&mut rx, 1_000, &d1[1..]); // incomplète
        feed(&mut rx, 2_000, &k5); // clé complète, plus récente
        assert_eq!(rx.pop_frame().unwrap().id, 5);
        assert!(rx.pop_frame().is_none());
        assert!(rx.stats.frames_lost >= 1);
    }
}
