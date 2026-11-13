//! Simulateur de lien déterministe (PRNG à graine) : perte, gigue, réordonnancement,
//! duplication, limite de débit avec file à queue-drop. Sert aux tests et aux mesures.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::{Endpoint, Micros};

/// PRNG xorshift64* (déterministe, sans dépendance).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniforme dans [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next_u64() as u8;
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// Probabilité de perte uniforme (0..1).
    pub loss: f64,
    /// Perte en rafales (modèle Gilbert-Elliott) : (P(bon->mauvais), P(mauvais->bon), perte dans l'état mauvais).
    pub burst: Option<(f64, f64, f64)>,
    pub delay_us: Micros,
    /// Gigue uniforme ajoutée dans [0, jitter_us].
    pub jitter_us: Micros,
    /// Probabilité qu'un paquet soit retardé de `reorder_extra_us` (il se fera doubler).
    pub reorder: f64,
    pub reorder_extra_us: Micros,
    pub duplicate: f64,
    /// Capacité du lien (bits/s) ; `None` = illimité.
    pub rate_bps: Option<u64>,
    /// Taille de la file avant le goulot (octets) ; queue-drop au-delà.
    pub queue_bytes: usize,
    pub seed: u64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            loss: 0.0,
            burst: None,
            delay_us: 0,
            jitter_us: 0,
            reorder: 0.0,
            reorder_extra_us: 0,
            duplicate: 0.0,
            rate_bps: None,
            queue_bytes: 128 * 1024,
            seed: 1,
        }
    }
}

impl LinkConfig {
    pub fn lossy(loss: f64, seed: u64) -> Self {
        Self { loss, delay_us: 10_000, jitter_us: 3_000, seed, ..Default::default() }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LinkStats {
    pub sent: u64,
    pub delivered: u64,
    pub lost: u64,
    pub queue_dropped: u64,
    pub duplicated: u64,
    pub bytes_sent: u64,
}

pub struct LinkSim {
    cfg: LinkConfig,
    rng: Rng,
    heap: BinaryHeap<Reverse<(Micros, u64, Vec<u8>)>>,
    seq: u64,
    free_at: Micros,
    bad: bool,
    pub stats: LinkStats,
}

impl LinkSim {
    pub fn new(cfg: LinkConfig) -> Self {
        Self {
            rng: Rng::new(cfg.seed),
            cfg,
            heap: BinaryHeap::new(),
            seq: 0,
            free_at: 0,
            bad: false,
            stats: LinkStats::default(),
        }
    }

    pub fn config_mut(&mut self) -> &mut LinkConfig {
        &mut self.cfg
    }

    fn lose(&mut self) -> bool {
        let mut p = self.cfg.loss;
        if let Some((enter, leave, bad_loss)) = self.cfg.burst {
            self.bad = if self.bad { self.rng.f64() >= leave } else { self.rng.f64() < enter };
            if self.bad {
                p = p.max(bad_loss);
            }
        }
        self.rng.f64() < p
    }

    fn push(&mut self, arrival: Micros, pkt: Vec<u8>) {
        self.seq += 1;
        self.heap.push(Reverse((arrival, self.seq, pkt)));
    }

    /// Injecte un paquet dans le lien à l'instant `now`.
    pub fn send(&mut self, now: Micros, pkt: Vec<u8>) {
        self.stats.sent += 1;
        self.stats.bytes_sent += pkt.len() as u64;
        if self.lose() {
            self.stats.lost += 1;
            return;
        }
        let mut depart = now;
        if let Some(rate) = self.cfg.rate_bps {
            let start = self.free_at.max(now);
            let backlog = ((start - now) as f64 * rate as f64 / 8e6) as usize;
            if backlog + pkt.len() > self.cfg.queue_bytes {
                self.stats.queue_dropped += 1;
                return;
            }
            self.free_at = start + (pkt.len() as f64 * 8e6 / rate as f64).ceil() as Micros;
            depart = self.free_at;
        }
        let mut arrival = depart + self.cfg.delay_us + self.rng.below(self.cfg.jitter_us + 1);
        if self.cfg.reorder > 0.0 && self.rng.f64() < self.cfg.reorder {
            arrival += self.cfg.reorder_extra_us;
        }
        if self.cfg.duplicate > 0.0 && self.rng.f64() < self.cfg.duplicate {
            self.stats.duplicated += 1;
            let a2 = arrival + self.rng.below(self.cfg.jitter_us + 2000);
            self.push(a2, pkt.clone());
        }
        self.push(arrival, pkt);
    }

    /// Prochain paquet arrivé à `now`.
    pub fn poll(&mut self, now: Micros) -> Option<Vec<u8>> {
        if self.heap.peek().is_some_and(|Reverse((t, _, _))| *t <= now) {
            self.stats.delivered += 1;
            return self.heap.pop().map(|Reverse((_, _, p))| p);
        }
        None
    }

    pub fn next_arrival(&self) -> Option<Micros> {
        self.heap.peek().map(|Reverse((t, _, _))| *t)
    }

    pub fn in_flight(&self) -> usize {
        self.heap.len()
    }
}

/// Deux `Endpoint` reliés par deux liens simulés (a -> b et b -> a), horloge virtuelle.
pub struct Pair {
    pub now: Micros,
    pub a: Endpoint,
    pub b: Endpoint,
    pub ab: LinkSim,
    pub ba: LinkSim,
}

impl Pair {
    pub fn new(a: Endpoint, b: Endpoint, ab: LinkConfig, ba: LinkConfig) -> Self {
        Self { now: 1_000_000, a, b, ab: LinkSim::new(ab), ba: LinkSim::new(ba) }
    }

    /// Fait circuler tout ce qui peut l'être à l'instant courant puis avance l'horloge de `dt`.
    pub fn step(&mut self, dt: Micros) {
        let now = self.now;
        while let Some(p) = self.ab.poll(now) {
            self.b.handle_datagram(now, &p);
        }
        while let Some(p) = self.ba.poll(now) {
            self.a.handle_datagram(now, &p);
        }
        while let Some(p) = self.a.poll_transmit(now) {
            self.ab.send(now, p);
        }
        while let Some(p) = self.b.poll_transmit(now) {
            self.ba.send(now, p);
        }
        self.now += dt;
    }

    pub fn run_for(&mut self, duration: Micros, dt: Micros) {
        let end = self.now + duration;
        while self.now < end {
            self.step(dt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_applies_loss() {
        let run = || {
            let mut l = LinkSim::new(LinkConfig::lossy(0.2, 42));
            for i in 0..10_000u32 {
                l.send(0, i.to_le_bytes().to_vec());
            }
            let mut out = Vec::new();
            while let Some(p) = l.poll(1_000_000) {
                out.push(u32::from_le_bytes(p.try_into().unwrap()));
            }
            out
        };
        let a = run();
        assert_eq!(a, run());
        let frac = 1.0 - a.len() as f64 / 10_000.0;
        assert!((0.18..0.22).contains(&frac), "{frac}");
    }

    #[test]
    fn jitter_reorders_and_duplicates() {
        let mut l = LinkSim::new(LinkConfig {
            delay_us: 1000,
            jitter_us: 5000,
            reorder: 0.1,
            reorder_extra_us: 20_000,
            duplicate: 0.1,
            seed: 3,
            ..Default::default()
        });
        for i in 0..1000u32 {
            l.send(i as u64 * 100, i.to_le_bytes().to_vec());
        }
        let mut out = Vec::new();
        while let Some(p) = l.poll(10_000_000) {
            out.push(u32::from_le_bytes(p.try_into().unwrap()));
        }
        assert!(out.len() > 1050);
        assert!(out.windows(2).any(|w| w[0] > w[1]), "pas de réordonnancement");
    }

    #[test]
    fn rate_limit_throttles_and_drops() {
        let mut l = LinkSim::new(LinkConfig { rate_bps: Some(8_000_000), queue_bytes: 20_000, ..Default::default() });
        // 100 paquets de 1000 octets d'un coup = 100 ko ; le lien en fait passer ~20 ko de file.
        for _ in 0..100 {
            l.send(0, vec![0; 1000]);
        }
        assert!(l.stats.queue_dropped >= 70, "{:?}", l.stats);
        let mut last = 0;
        let mut n = 0;
        for t in (0..1_000_000).step_by(1000) {
            while l.poll(t).is_some() {
                n += 1;
                last = t;
            }
        }
        assert!(n <= 30);
        assert!(last >= 15_000, "débit trop rapide : {last}");
    }
}
