//! Jitter buffer adaptatif au niveau paquet (un paquet = une trame Opus).
//!
//! Le buffer ne connait pas l'horloge : c'est le consommateur (peripherique audio) qui le cadence
//! en appelant [`JitterBuffer::pop`] une fois par duree de trame. Cela fait de l'horloge du
//! peripherique de lecture la reference ; la derive host/client se traduit par une variation
//! lente de la profondeur (`depth`), que le lecteur compense en modulant le reechantillonneur.
//!
//! Comportement :
//! * amorcage : on attend `target` paquets avant de rendre le premier ;
//! * reordonnancement : un paquet manquant est attendu jusqu'a son echeance de lecture (= la profondeur
//!   amorcee absorbe le desordre) ; arrive apres, il est ecarte (`late`) ;
//! * perte : `Pop::Lost` (le decodeur fait du PLC, ou du FEC si `fec` contient le paquet suivant) ;
//! * sous-alimentation : PLC et `target` augmente de 1 trame ; apres 5 sous-alimentations d'affilee
//!   on repasse en `Buffering` (silence) jusqu'au reamorcage ;
//! * adaptation : `target` suit l'estimation de gigue RFC 3550 (croissance immediate, decroissance
//!   d'une trame apres ~3 s calmes) ;
//! * debordement (rafale, horloge distante plus rapide) : on saute les plus anciens paquets pour
//!   revenir a `target`.

use std::time::Instant;

#[derive(Clone, Debug)]
pub struct JitterConfig {
    /// Duree d'une trame (ms).
    pub frame_ms: f64,
    /// Profondeur minimale / initiale (ms). ~20 ms par defaut.
    pub min_ms: f64,
    /// Profondeur maximale (ms) ; au-dela on saute des paquets.
    pub max_ms: f64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { frame_ms: 10.0, min_ms: 20.0, max_ms: 80.0 }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pop {
    /// Pas encore assez de paquets (ou stream arrete) : jouer du silence.
    Buffering,
    /// Le prochain paquet dans l'ordre.
    Packet(Vec<u8>),
    /// Trame manquante. `fec` = paquet suivant (s'il est deja la) dont le FEC peut reconstituer la trame ;
    /// sinon decoder avec `None` (PLC).
    Lost { fec: Option<Vec<u8>> },
}

#[derive(Clone, Debug, Default)]
pub struct JitterStats {
    pub received: u64,
    pub duplicates: u64,
    pub late: u64,
    pub lost: u64,
    pub underruns: u64,
    pub overflow_drops: u64,
    pub rebuffers: u64,
    pub target_frames: usize,
    pub depth: usize,
    pub jitter_ms: f64,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    packets: Vec<(u32, Vec<u8>)>,
    next_seq: u32,
    started: bool,
    target: usize,
    min_frames: usize,
    max_frames: usize,
    jitter_ms: f64,
    last_arrival: Option<(u32, f64)>,
    consec_underrun: u32,
    quiet_pops: u32,
    stats: JitterStats,
    epoch: Instant,
}

#[inline]
fn dist(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        let min_frames = ((cfg.min_ms / cfg.frame_ms).ceil() as usize).max(1);
        let max_frames = ((cfg.max_ms / cfg.frame_ms).ceil() as usize).max(min_frames + 1);
        Self {
            cfg,
            packets: Vec::new(),
            next_seq: 0,
            started: false,
            target: min_frames,
            min_frames,
            max_frames,
            jitter_ms: 0.0,
            last_arrival: None,
            consec_underrun: 0,
            quiet_pops: 0,
            stats: JitterStats::default(),
            epoch: Instant::now(),
        }
    }

    pub fn depth(&self) -> usize {
        self.packets.len()
    }

    pub fn target_frames(&self) -> usize {
        self.target
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn stats(&self) -> JitterStats {
        let mut s = self.stats.clone();
        s.target_frames = self.target;
        s.depth = self.packets.len();
        s.jitter_ms = self.jitter_ms;
        s
    }

    /// Insere un paquet recu maintenant.
    pub fn push(&mut self, seq: u32, data: Vec<u8>) {
        let now = self.epoch.elapsed().as_secs_f64() * 1000.0;
        self.push_at(seq, data, now);
    }

    /// Insere un paquet avec une date d'arrivee explicite (ms, horloge monotone quelconque) : testable.
    pub fn push_at(&mut self, seq: u32, data: Vec<u8>, now_ms: f64) {
        self.stats.received += 1;

        // estimation de gigue (RFC 3550) sur le paquet le plus recent en numero de sequence
        match self.last_arrival {
            Some((ls, lt)) => {
                let ds = dist(seq, ls);
                if ds > 0 {
                    let d = (now_ms - lt) - ds as f64 * self.cfg.frame_ms;
                    self.jitter_ms += (d.abs() - self.jitter_ms) / 16.0;
                    self.last_arrival = Some((seq, now_ms));
                }
            }
            None => self.last_arrival = Some((seq, now_ms)),
        }

        if self.started {
            let d = dist(seq, self.next_seq);
            if d < 0 {
                if d > -(self.max_frames as i32) * 8 {
                    self.stats.late += 1;
                    // un paquet en retard de peu : la gigue depasse la profondeur courante
                    if d >= -3 {
                        self.grow();
                    }
                    return;
                }
                self.restart(); // retour en arriere massif : nouveau flux
            } else if d > (self.max_frames as i32) * 8 {
                self.restart(); // saut en avant massif : nouveau flux
            }
        }
        if self.packets.iter().any(|(s, _)| *s == seq) {
            self.stats.duplicates += 1;
            return;
        }
        self.packets.push((seq, data));

        if self.started {
            self.trim_overflow();
        }
        self.adapt_target_up();
    }

    fn restart(&mut self) {
        self.packets.clear();
        self.started = false;
        self.consec_underrun = 0;
        self.last_arrival = None;
    }

    fn highest_span(&self) -> usize {
        self.packets
            .iter()
            .map(|(s, _)| dist(*s, self.next_seq))
            .max()
            .map(|d| (d + 1).max(0) as usize)
            .unwrap_or(0)
    }

    fn trim_overflow(&mut self) {
        let span = self.highest_span();
        if span > self.max_frames {
            let skip = (span - self.target) as u32;
            let before = self.packets.len();
            self.next_seq = self.next_seq.wrapping_add(skip);
            let ns = self.next_seq;
            self.packets.retain(|(s, _)| dist(*s, ns) >= 0);
            self.stats.overflow_drops += (before - self.packets.len()) as u64;
        }
    }

    fn desired_frames(&self) -> usize {
        let ms = self.cfg.min_ms.max(self.cfg.frame_ms + 3.0 * self.jitter_ms);
        ((ms / self.cfg.frame_ms).ceil() as usize).clamp(self.min_frames, self.max_frames)
    }

    fn adapt_target_up(&mut self) {
        let d = self.desired_frames();
        if d > self.target {
            self.target = d;
            self.quiet_pops = 0;
        }
    }

    fn grow(&mut self) {
        if self.target < self.max_frames {
            self.target += 1;
        }
        self.quiet_pops = 0;
    }

    /// A appeler une fois par duree de trame, au rythme du peripherique de sortie.
    pub fn pop(&mut self) -> Pop {
        if !self.started {
            if self.packets.len() < self.target || self.packets.is_empty() {
                return Pop::Buffering;
            }
            let first = self.packets[0].0;
            let earliest = self.packets.iter().map(|(s, _)| *s).min_by_key(|s| dist(*s, first)).unwrap();
            self.next_seq = earliest;
            self.started = true;
            self.consec_underrun = 0;
            // gros stock accumule pendant un trou (rafale a la reprise) : on saute le plus ancien
            self.trim_overflow();
        }

        let ns = self.next_seq;
        if let Some(i) = self.packets.iter().position(|(s, _)| *s == ns) {
            let (_, data) = self.packets.swap_remove(i);
            self.next_seq = ns.wrapping_add(1);
            self.consec_underrun = 0;
            self.note_quiet_pop();
            return Pop::Packet(data);
        }

        if self.packets.is_empty() {
            // sous-alimentation : rien du tout en stock
            self.stats.underruns += 1;
            self.consec_underrun += 1;
            if self.consec_underrun == 1 {
                self.grow(); // une seule hausse par trou (un arret de flux n'est pas de la gigue)
            }
            if self.consec_underrun >= 5 {
                // flux arrete : on repart d'une cible issue de la gigue mesuree, pas des trous
                self.started = false;
                self.consec_underrun = 0;
                self.stats.rebuffers += 1;
                self.target = self.desired_frames();
                return Pop::Buffering;
            }
            self.next_seq = ns.wrapping_add(1);
            self.stats.lost += 1;
            return Pop::Lost { fec: None };
        }

        // paquet manquant alors que des suivants sont la : perdu (ou trop en retard)
        self.stats.lost += 1;
        self.consec_underrun = 0;
        let nxt = ns.wrapping_add(1);
        let fec = self.packets.iter().find(|(s, _)| *s == nxt).map(|(_, d)| d.clone());
        self.next_seq = nxt;
        self.quiet_pops = 0;
        Pop::Lost { fec }
    }

    fn note_quiet_pop(&mut self) {
        self.quiet_pops += 1;
        if self.quiet_pops >= 300 {
            self.quiet_pops = 0;
            if self.desired_frames() < self.target {
                self.target -= 1;
            }
        }
    }
}

/// Correction de derive d'horloge : transforme l'ecart (profondeur lissee - cible) du jitter buffer en
/// correction relative du rapport de reechantillonnage (`ratio_effectif = nominal * (1 + adj)`).
/// Proportionnel avec zone morte de 0,5 trame et saturation a +-0,5 % (inaudible : ~8 cents).
pub struct DriftController {
    ema: f64,
    adj: f64,
    /// Constante de lissage par appel (0.01 a ~100 appels/s = ~1 s).
    pub alpha: f64,
    pub deadband_frames: f64,
    pub gain: f64,
    pub max_adj: f64,
}

impl Default for DriftController {
    fn default() -> Self {
        Self { ema: 0.0, adj: 0.0, alpha: 0.01, deadband_frames: 0.5, gain: 0.004, max_adj: 0.005 }
    }
}

impl DriftController {
    /// `depth` et `target` en trames. `active=false` (amorcage/silence) remet l'etat a zero.
    pub fn update(&mut self, depth: f64, target: f64, active: bool) -> f64 {
        if !active {
            self.ema = depth;
            self.adj = 0.0;
            return 0.0;
        }
        self.ema += (depth - self.ema) * self.alpha;
        let err = self.ema - target;
        self.adj = if err.abs() < self.deadband_frames {
            0.0
        } else {
            (err.signum() * (err.abs() - self.deadband_frames) * self.gain).clamp(-self.max_adj, self.max_adj)
        };
        self.adj
    }
    pub fn adjust(&self) -> f64 {
        self.adj
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(seq: u32) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }
    fn seq_of(p: &[u8]) -> u32 {
        u32::from_le_bytes(p.try_into().unwrap())
    }

    /// Simule : `arrivals` = (date d'arrivee ms, seq). Lecture : un pop toutes les 10 ms a partir de `play_start`.
    fn simulate(jb: &mut JitterBuffer, mut arrivals: Vec<(f64, u32)>, play_start: f64, n_pops: usize) -> Vec<Pop> {
        arrivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut it = arrivals.into_iter().peekable();
        let mut out = Vec::new();
        for k in 0..n_pops {
            let t = play_start + 10.0 * k as f64;
            while let Some(&(ta, s)) = it.peek() {
                if ta <= t {
                    jb.push_at(s, pk(s), ta);
                    it.next();
                } else {
                    break;
                }
            }
            out.push(jb.pop());
        }
        out
    }

    fn packets(out: &[Pop]) -> Vec<u32> {
        out.iter().filter_map(|p| if let Pop::Packet(d) = p { Some(seq_of(d)) } else { None }).collect()
    }

    #[test]
    fn in_order_no_loss() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let arr: Vec<_> = (0..50).map(|i| (i as f64 * 10.0, i)).collect();
        let out = simulate(&mut jb, arr, 25.0, 45);
        let got = packets(&out);
        assert_eq!(got, (0..got.len() as u32).collect::<Vec<_>>());
        assert!(got.len() >= 40);
        let s = jb.stats();
        assert_eq!((s.lost, s.late, s.underruns), (0, 0, 0));
    }

    #[test]
    fn reordering_is_fixed() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // paquets 3k+1 et 3k+2 permutes (le 2e arrive 4 ms avant le 1er)
        let arr: Vec<_> = (0..60u32)
            .map(|i| {
                let t = i as f64 * 10.0;
                let t = if i % 6 == 3 { t + 6.0 } else if i % 6 == 4 { t - 6.0 } else { t };
                (t, i)
            })
            .collect();
        let out = simulate(&mut jb, arr, 25.0, 55);
        let got = packets(&out);
        assert_eq!(got, (0..got.len() as u32).collect::<Vec<_>>(), "sortie non ordonnee");
        assert_eq!(jb.stats().lost, 0);
        assert!(got.len() >= 50);
    }

    #[test]
    fn loss_gives_plc_and_fec_hint() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let lost = [8u32, 9, 20];
        let arr: Vec<_> = (0..40u32).filter(|i| !lost.contains(i)).map(|i| (i as f64 * 10.0, i)).collect();
        let out = simulate(&mut jb, arr, 25.0, 38);
        // sortie alignee sur la sequence : out[k] correspond a seq k (la lecture demarre sur seq 0)
        for (k, p) in out.iter().enumerate() {
            let k = k as u32;
            match p {
                Pop::Packet(d) => assert_eq!(seq_of(d), k),
                Pop::Lost { fec } => {
                    assert!(lost.contains(&k), "perte inattendue a {k}");
                    match k {
                        8 => assert!(fec.is_none()), // 9 aussi perdu
                        9 => assert_eq!(seq_of(fec.as_ref().unwrap()), 10),
                        20 => assert_eq!(seq_of(fec.as_ref().unwrap()), 21),
                        _ => unreachable!(),
                    }
                }
                Pop::Buffering => panic!("buffering inattendu a {k}"),
            }
        }
        assert_eq!(jb.stats().lost, 3);
    }

    #[test]
    fn late_packet_is_dropped_not_replayed() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arr: Vec<_> = (0..30u32).map(|i| (i as f64 * 10.0, i)).collect();
        // paquet 10 arrive 100 ms trop tard
        arr[10].0 += 100.0;
        let out = simulate(&mut jb, arr, 25.0, 28);
        let got = packets(&out);
        assert!(!got.contains(&10) || out.iter().filter(|p| matches!(p, Pop::Lost { .. })).count() == 0);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), got.len(), "doublon");
        assert!(jb.stats().late >= 1 || jb.stats().lost >= 1);
    }

    #[test]
    fn long_gap_rebuffers_then_resumes_on_new_sequence() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arr: Vec<_> = (0..20u32).map(|i| (i as f64 * 10.0, i)).collect();
        // trou de 300 ms puis reprise
        arr.extend((20..60u32).map(|i| (i as f64 * 10.0 + 300.0, i)));
        let out = simulate(&mut jb, arr, 25.0, 120);
        assert!(jb.stats().rebuffers >= 1);
        let got = packets(&out);
        assert!(got.contains(&59) || got.contains(&58), "reprise non observee : {got:?}");
        assert!(got.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn burst_overflow_is_bounded() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for i in 0..2u32 {
            jb.push_at(i, pk(i), 0.0);
        }
        assert!(matches!(jb.pop(), Pop::Packet(_)));
        // rafale de 40 paquets d'un coup
        for i in 2..42u32 {
            jb.push_at(i, pk(i), 5.0);
        }
        assert!(jb.depth() <= 8, "depth {}", jb.depth());
        assert!(jb.stats().overflow_drops > 0);
        // on retombe sur une suite ordonnee et reciproquement contigue
        let mut prev = None;
        while let Pop::Packet(d) = jb.pop() {
            let s = seq_of(&d);
            if let Some(p) = prev {
                assert_eq!(s, p + 1);
            }
            prev = Some(s);
        }
    }

    #[test]
    fn target_grows_with_jitter_and_stays_in_bounds() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.target_frames(), 2);
        let mut t = 0.0;
        for i in 0..200u32 {
            // arrivees tres irregulieres : +-15 ms
            let j = if i % 2 == 0 { 15.0 } else { -15.0 };
            t = i as f64 * 10.0 + j;
            jb.push_at(i, pk(i), t);
        }
        let _ = t;
        assert!(jb.target_frames() > 2, "target {}", jb.target_frames());
        assert!(jb.target_frames() <= 8);
        assert!(jb.stats().jitter_ms > 5.0);
    }

    #[test]
    fn sequence_wraparound() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let base = u32::MAX - 5;
        let arr: Vec<_> = (0..20u32).map(|i| (i as f64 * 10.0, base.wrapping_add(i))).collect();
        let out = simulate(&mut jb, arr, 25.0, 18);
        let got = packets(&out);
        assert!(got.len() >= 15);
        for w in got.windows(2) {
            assert_eq!(w[1], w[0].wrapping_add(1));
        }
    }

    #[test]
    fn duplicates_ignored() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        jb.push_at(0, pk(0), 0.0);
        jb.push_at(0, pk(0), 1.0);
        jb.push_at(1, pk(1), 10.0);
        assert_eq!(jb.stats().duplicates, 1);
        assert_eq!(jb.depth(), 2);
    }

    #[test]
    fn drift_controller_tracks_fast_and_slow_sender_clocks() {
        for eps in [0.0005f64, -0.0005, 0.003, -0.003] {
            let mut dc = DriftController::default();
            let target = 2.0;
            let mut depth = target;
            let mut adj = 0.0;
            let (mut min_d, mut max_d) = (f64::MAX, f64::MIN);
            // 120 s d'appels a 100 Hz ; l'emetteur produit (1+eps) trame par tick, le lecteur en consomme (1+adj)
            for tick in 0..12_000 {
                depth += (1.0 + eps) - (1.0 + adj);
                adj = dc.update(depth, target, true);
                if tick > 6_000 {
                    min_d = min_d.min(depth);
                    max_d = max_d.max(depth);
                }
            }
            assert!(adj.abs() <= 0.005 + 1e-12);
            assert!(min_d > 0.0 && max_d < 8.0, "eps {eps}: profondeur {min_d:.2}..{max_d:.2} hors bornes");
            // la correction a bien converge vers la derive de l'horloge (si elle est dans la plage corrigeable)
            if eps.abs() >= 0.003 {
                assert!((adj - eps).abs() < 0.0005, "eps {eps}: adj {adj}");
            }
        }
    }
}
