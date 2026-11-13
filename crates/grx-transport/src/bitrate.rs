//! Contrôle de débit : propose à l'encodeur un bitrate cible et un taux de FEC.
//!
//! Algorithme (inspiré de GCC, côté émetteur, piloté par les rapports du récepteur) :
//! - perte forte (> `loss_decrease_threshold`) -> décroissance multiplicative `1 - loss/2` ;
//! - délai de file d'attente (RTT lissé - RTT min) trop grand -> décroissance à `decrease_factor` x
//!   le débit reçu (au plus `decrease_factor` x le budget courant) ;
//! - sinon, si la perte est couverte par la FEC -> croissance (multiplicative douce, au moins
//!   `min_increase_bps_per_sec`), plafonnée à 1,5x le débit réellement reçu ;
//! - deux décroissances sont espacées d'au moins `holdoff_us` (laisser l'effet se propager).
//!
//! Le budget géré ici est le débit TOTAL envoyé (média + FEC). `encoder_bps()` en déduit le
//! bitrate à donner à l'encodeur.

use crate::Micros;

#[derive(Clone, Debug)]
pub struct BitrateConfig {
    pub min_bps: u64,
    pub max_bps: u64,
    pub start_bps: u64,
    pub loss_decrease_threshold: f32,
    pub loss_increase_threshold: f32,
    pub queue_delay_threshold_us: Micros,
    pub decrease_factor: f64,
    /// Croissance relative par seconde (0.10 = +10 %/s).
    pub increase_per_sec: f64,
    pub min_increase_bps_per_sec: f64,
    pub holdoff_us: Micros,
    pub fec_base_permille: u16,
    pub fec_max_permille: u16,
}

impl Default for BitrateConfig {
    fn default() -> Self {
        Self {
            min_bps: 500_000,
            max_bps: 200_000_000,
            start_bps: 20_000_000,
            loss_decrease_threshold: 0.10,
            loss_increase_threshold: 0.02,
            queue_delay_threshold_us: 25_000,
            decrease_factor: 0.85,
            increase_per_sec: 0.10,
            min_increase_bps_per_sec: 1_000_000.0,
            holdoff_us: 200_000,
            fec_base_permille: 40,
            fec_max_permille: 300,
        }
    }
}

/// Mesure réseau fournie au contrôleur à chaque rapport du récepteur.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkSample {
    /// Fraction de paquets média perdus (0..1) sur l'intervalle.
    pub loss: f32,
    pub rtt_us: Option<Micros>,
    pub min_rtt_us: Option<Micros>,
    /// Débit reçu par le pair (bits/s), 0 si inconnu.
    pub recv_rate_bps: u64,
}

#[derive(Clone, Debug)]
pub struct BitrateController {
    cfg: BitrateConfig,
    total: f64,
    loss_ema: Option<f32>,
    last_update: Option<Micros>,
    last_decrease: Option<Micros>,
    pub decreases: u64,
    pub increases: u64,
    /// `max_bps` d'origine (avant un éventuel plafond posé par [`BitrateController::set_cap`]).
    base_max_bps: u64,
}

impl BitrateController {
    pub fn new(cfg: BitrateConfig) -> Self {
        let total = cfg.start_bps.clamp(cfg.min_bps, cfg.max_bps) as f64;
        let base_max_bps = cfg.max_bps;
        Self { cfg, total, loss_ema: None, last_update: None, last_decrease: None, decreases: 0, increases: 0, base_max_bps }
    }

    /// Plafond à chaud du budget total (ex. « bande passante max » choisie par le client) :
    /// `min(cap, max_bps d'origine)`, jamais sous `min_bps`. `None` rétablit le maximum d'origine.
    /// Le budget courant est ramené sous le plafond immédiatement.
    pub fn set_cap(&mut self, cap_bps: Option<u64>) {
        let max = cap_bps.map_or(self.base_max_bps, |c| c.min(self.base_max_bps));
        self.cfg.max_bps = max.max(self.cfg.min_bps);
        self.total = self.total.clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
    }

    /// Plafond effectif du budget total (bits/s).
    pub fn max_bps(&self) -> u64 {
        self.cfg.max_bps
    }

    pub fn config(&self) -> &BitrateConfig {
        &self.cfg
    }

    /// Budget total (média + FEC) en bits/s.
    pub fn total_bps(&self) -> u64 {
        self.total as u64
    }

    /// Taux de FEC suggéré (pour-mille des données), fonction de la perte récente.
    pub fn fec_permille(&self) -> u16 {
        let loss = self.loss_ema.unwrap_or(0.0) as f64;
        let p = self.cfg.fec_base_permille as f64 + 2500.0 * loss;
        p.clamp(self.cfg.fec_base_permille as f64, self.cfg.fec_max_permille as f64) as u16
    }

    /// Bitrate à donner à l'encodeur (le budget total moins la redondance FEC).
    pub fn encoder_bps(&self) -> u64 {
        (self.total / (1.0 + self.fec_permille() as f64 / 1000.0)) as u64
    }

    pub fn loss_estimate(&self) -> f32 {
        self.loss_ema.unwrap_or(0.0)
    }

    pub fn on_sample(&mut self, now: Micros, s: NetworkSample) {
        let dt = match self.last_update {
            Some(t) => (now.saturating_sub(t)).clamp(1_000, 1_000_000) as f64 / 1e6,
            None => 0.1,
        };
        self.last_update = Some(now);
        let loss = s.loss.clamp(0.0, 1.0);
        let ema = match self.loss_ema {
            Some(e) => 0.8 * e + 0.2 * loss,
            None => loss,
        };
        self.loss_ema = Some(ema);

        let qd = match (s.rtt_us, s.min_rtt_us) {
            (Some(r), Some(m)) => r.saturating_sub(m),
            _ => 0,
        };
        let can_decrease = self.last_decrease.is_none_or(|t| now.saturating_sub(t) >= self.cfg.holdoff_us);
        let fec = self.fec_permille() as f32 / 1000.0;
        let inc_loss_limit = self.cfg.loss_increase_threshold.max(0.6 * fec);

        if loss > self.cfg.loss_decrease_threshold {
            if can_decrease {
                self.total *= 1.0 - 0.5 * loss.min(0.5) as f64;
                self.mark_decrease(now);
            }
        } else if qd > self.cfg.queue_delay_threshold_us {
            if can_decrease {
                // Comme GCC : on s'aligne sur le débit réellement écoulé par le goulot
                // (évite de s'effondrer bien en dessous de la capacité pendant que la file se vide).
                let by_factor = self.total * self.cfg.decrease_factor;
                let by_rate = if s.recv_rate_bps > 0 { s.recv_rate_bps as f64 * self.cfg.decrease_factor } else { by_factor };
                // (jamais en dessous de la moitié de `by_factor` en une seule étape)
                self.total = by_factor.min(by_rate.max(by_factor * 0.5));
                self.mark_decrease(now);
            }
        } else if loss <= inc_loss_limit && qd <= self.cfg.queue_delay_threshold_us / 2 {
            let inc = (self.total * self.cfg.increase_per_sec).max(self.cfg.min_increase_bps_per_sec) * dt;
            let mut next = self.total + inc;
            if s.recv_rate_bps > 0 {
                let cap = s.recv_rate_bps as f64 * 1.5 + 1_000_000.0;
                next = next.min(cap.max(self.total));
            }
            if next > self.total {
                self.increases += 1;
            }
            self.total = next;
        }
        self.total = self.total.clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
    }

    fn mark_decrease(&mut self, now: Micros) {
        self.last_decrease = Some(now);
        self.decreases += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(rtt: u64) -> NetworkSample {
        NetworkSample { loss: 0.0, rtt_us: Some(rtt), min_rtt_us: Some(rtt), recv_rate_bps: 0 }
    }

    #[test]
    fn increases_when_clean_up_to_max() {
        let mut c = BitrateController::new(BitrateConfig { start_bps: 5_000_000, max_bps: 50_000_000, ..Default::default() });
        let start = c.total_bps();
        for i in 1..=600u64 {
            c.on_sample(i * 100_000, clean(10_000));
        }
        assert!(c.total_bps() > start * 2);
        assert_eq!(c.total_bps(), 50_000_000);
        assert!(c.encoder_bps() < c.total_bps());
    }

    #[test]
    fn heavy_loss_decreases_with_holdoff() {
        let mut c = BitrateController::new(BitrateConfig::default());
        let s = NetworkSample { loss: 0.3, rtt_us: Some(20_000), min_rtt_us: Some(20_000), recv_rate_bps: 0 };
        let before = c.total_bps();
        c.on_sample(1_000_000, s);
        let after1 = c.total_bps();
        assert!(after1 < before * 9 / 10, "{before} -> {after1}");
        c.on_sample(1_100_000, s); // dans le holdoff : pas de 2e décroissance
        assert_eq!(c.total_bps(), after1);
        c.on_sample(1_300_000, s);
        assert!(c.total_bps() < after1);
        assert_eq!(c.decreases, 2);
    }

    #[test]
    fn queue_delay_decreases_and_floor_holds() {
        let mut c = BitrateController::new(BitrateConfig::default());
        let s = NetworkSample { loss: 0.0, rtt_us: Some(120_000), min_rtt_us: Some(20_000), recv_rate_bps: 0 };
        let before = c.total_bps();
        c.on_sample(1_000_000, s);
        assert!(c.total_bps() < before);
        for i in 0..200u64 {
            c.on_sample(2_000_000 + i * 300_000, s);
        }
        assert_eq!(c.total_bps(), c.config().min_bps);
    }

    #[test]
    fn fec_follows_loss_and_encoder_rate_accounts_for_it() {
        let mut c = BitrateController::new(BitrateConfig::default());
        assert_eq!(c.fec_permille(), 40);
        for i in 1..=50u64 {
            c.on_sample(i * 100_000, NetworkSample { loss: 0.05, rtt_us: Some(10_000), min_rtt_us: Some(10_000), recv_rate_bps: 0 });
        }
        let f = c.fec_permille();
        assert!((140..=190).contains(&f), "{f}");
        assert!(c.encoder_bps() < c.total_bps());
        // Une perte de 5 % couverte par la FEC ne bloque pas la croissance.
        assert!(c.increases > 10);
    }

    #[test]
    fn cap_applies_immediately_and_can_be_lifted() {
        let mut c = BitrateController::new(BitrateConfig { start_bps: 40_000_000, max_bps: 50_000_000, ..Default::default() });
        c.set_cap(Some(8_000_000));
        assert_eq!(c.total_bps(), 8_000_000);
        for i in 1..=100u64 {
            c.on_sample(i * 100_000, clean(10_000));
        }
        assert_eq!(c.total_bps(), 8_000_000, "la croissance respecte le plafond");
        c.set_cap(Some(500_000_000)); // au-dessus du max d'origine : borné
        assert_eq!(c.max_bps(), 50_000_000);
        c.set_cap(Some(1)); // sous le minimum : borné
        assert_eq!(c.max_bps(), c.config().min_bps);
        c.set_cap(None);
        assert_eq!(c.max_bps(), 50_000_000);
        for i in 101..=400u64 {
            c.on_sample(i * 100_000, clean(10_000));
        }
        assert!(c.total_bps() > 8_000_000);
    }

    #[test]
    fn growth_is_capped_by_received_rate() {
        let mut c = BitrateController::new(BitrateConfig { start_bps: 10_000_000, ..Default::default() });
        for i in 1..=300u64 {
            c.on_sample(
                i * 100_000,
                NetworkSample { loss: 0.0, rtt_us: Some(10_000), min_rtt_us: Some(10_000), recv_rate_bps: 10_000_000 },
            );
        }
        assert!(c.total_bps() <= 16_100_000, "{}", c.total_bps());
    }
}
