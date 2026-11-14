//! Estimation réseau : RTT (RFC 6298), perte de paquets média (trous de `tseq`), débit reçu.

use crate::Micros;

/// RTT lissé + variance + minimum glissant (≈ latence de propagation, sert de référence
/// pour estimer le délai de file d'attente).
#[derive(Clone, Debug)]
pub struct RttEstimator {
    srtt: Option<f64>,
    rttvar: f64,
    min: Option<(Micros, Micros)>, // (valeur, instant de la mesure)
    last: Option<Micros>,
    pub samples: u64,
}

const MIN_WINDOW_US: Micros = 10_000_000;
/// Mesures pendant lesquelles une valeur initiale aberrante peut être remplacée.
const WARMUP_SAMPLES: u64 = 8;

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub fn new() -> Self {
        Self { srtt: None, rttvar: 0.0, min: None, last: None, samples: 0 }
    }

    pub fn on_sample(&mut self, now: Micros, rtt_us: Micros) {
        let r = rtt_us.max(1) as f64;
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            // Démarrage : une 1re mesure gonflée (pair occupé à ouvrir la fenêtre/sonder ses
            // décodeurs) mettait ~10 s à s'effacer (pings toutes les 0,5 s, gain 1/8) et affichait
            // un RTT de 10-250 ms en local. Pendant les 8 premières mesures, une mesure 4x plus
            // faible ré-amorce l'estimateur.
            Some(s) if self.samples < WARMUP_SAMPLES && r * 4.0 < s => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            Some(s) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (s - r).abs();
                self.srtt = Some(0.875 * s + 0.125 * r);
            }
        }
        self.last = Some(rtt_us);
        self.samples += 1;
        match self.min {
            Some((v, t)) if rtt_us >= v && now.saturating_sub(t) < MIN_WINDOW_US => {}
            _ => self.min = Some((rtt_us, now)),
        }
    }

    pub fn srtt(&self) -> Option<Micros> {
        self.srtt.map(|s| s as Micros)
    }
    pub fn min_rtt(&self) -> Option<Micros> {
        self.min.map(|m| m.0)
    }
    pub fn latest(&self) -> Option<Micros> {
        self.last
    }

    /// Délai de retransmission : srtt + 4*rttvar, borné.
    pub fn rto(&self, min_us: Micros, max_us: Micros, initial_us: Micros) -> Micros {
        match self.srtt {
            None => initial_us,
            Some(s) => ((s + 4.0 * self.rttvar) as Micros).clamp(min_us, max_us),
        }
    }
}

/// Mesure la perte des paquets média à partir des numéros `tseq` (u16 avec rebouclage).
/// Déduplique (la duplication réseau ne doit pas masquer la perte).
pub struct MediaRx {
    highest: u16,
    started: bool,
    seen: Vec<u64>, // 1024 bits
    base: u16,      // highest au dernier rapport
    received: u32,
    bytes: u64,
    pub total_received: u64,
    pub total_expected: u64,
}

const WIN: u16 = 1024;

impl Default for MediaRx {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaRx {
    pub fn new() -> Self {
        Self {
            highest: 0,
            started: false,
            seen: vec![0; (WIN / 64) as usize],
            base: 0,
            received: 0,
            bytes: 0,
            total_received: 0,
            total_expected: 0,
        }
    }

    fn bit(&self, s: u16) -> bool {
        let i = (s % WIN) as usize;
        self.seen[i / 64] >> (i % 64) & 1 != 0
    }
    fn set(&mut self, s: u16, v: bool) {
        let i = (s % WIN) as usize;
        if v {
            self.seen[i / 64] |= 1 << (i % 64);
        } else {
            self.seen[i / 64] &= !(1 << (i % 64));
        }
    }

    pub fn on_packet(&mut self, tseq: u16, len: usize) {
        self.bytes += len as u64;
        if !self.started {
            self.started = true;
            self.highest = tseq;
            self.base = tseq.wrapping_sub(1);
            self.set(tseq, true);
            self.received += 1;
            return;
        }
        let d = tseq.wrapping_sub(self.highest) as i16;
        if d > 0 {
            let d = d as u16;
            if d >= WIN {
                self.seen.iter_mut().for_each(|w| *w = 0);
            } else {
                for i in 1..=d {
                    self.set(self.highest.wrapping_add(i), false);
                }
            }
            self.highest = tseq;
            self.set(tseq, true);
            self.received += 1;
        } else {
            // paquet en retard (ou doublon)
            let back = self.highest.wrapping_sub(tseq);
            if back < WIN && !self.bit(tseq) {
                self.set(tseq, true);
                self.received += 1;
            }
        }
    }

    pub fn has_data(&self) -> bool {
        self.started && (self.received > 0)
    }

    /// Clôt l'intervalle : retourne (fraction perdue 0..1, octets reçus).
    pub fn close_interval(&mut self) -> (f32, u64) {
        let expected = self.highest.wrapping_sub(self.base) as u32;
        let loss = if expected == 0 {
            0.0
        } else {
            (1.0 - (self.received.min(expected) as f32 / expected as f32)).clamp(0.0, 1.0)
        };
        self.total_expected += expected as u64;
        self.total_received += self.received.min(expected) as u64;
        let bytes = self.bytes;
        self.base = self.highest;
        self.received = 0;
        self.bytes = 0;
        (loss, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_converges_and_rto_is_bounded() {
        let mut r = RttEstimator::new();
        assert_eq!(r.rto(20_000, 1_000_000, 200_000), 200_000);
        for i in 0..50 {
            r.on_sample(i * 1000, 40_000);
        }
        let s = r.srtt().unwrap();
        assert!((39_000..=41_000).contains(&s), "{s}");
        assert_eq!(r.min_rtt(), Some(40_000));
        let rto = r.rto(20_000, 1_000_000, 200_000);
        assert!((40_000..=60_000).contains(&rto), "{rto}");
        r.on_sample(60_000, 30_000);
        assert_eq!(r.min_rtt(), Some(30_000));
    }

    #[test]
    fn loss_is_measured_and_dedup() {
        let mut m = MediaRx::new();
        // 100 paquets émis (seq 65500..), on en reçoit 90, avec doublons et réordonnancement
        let mut got = 0;
        for i in 0..100u16 {
            let s = 65_500u16.wrapping_add(i);
            if i % 10 == 5 {
                continue;
            }
            m.on_packet(s, 1000);
            m.on_packet(s, 1000); // doublon
            got += 1;
        }
        assert_eq!(got, 90);
        let (loss, bytes) = m.close_interval();
        assert!((loss - 0.10).abs() < 0.02, "{loss}");
        assert!(bytes >= 90_000);
        // paquet en retard comptabilisé une seule fois
        let mut m = MediaRx::new();
        for s in [0u16, 1, 3, 4, 2] {
            m.on_packet(s, 10);
        }
        let (loss, _) = m.close_interval();
        assert_eq!(loss, 0.0);
    }
}

#[cfg(test)]
#[path = "netstats_warmup_tests.rs"]
mod warmup_tests;
