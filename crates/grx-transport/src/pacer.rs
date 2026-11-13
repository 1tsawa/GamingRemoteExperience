//! Pacing côté émetteur : seau à jetons (évite les rafales qui saturent les files du réseau).

use crate::Micros;

#[derive(Clone, Debug)]
pub struct Pacer {
    rate_bps: f64,
    burst_bytes: f64,
    tokens: f64,
    last: Option<Micros>,
}

impl Pacer {
    /// `burst_bytes` : rafale maximale autorisée (au moins la taille d'un paquet).
    pub fn new(rate_bps: u64, burst_bytes: usize) -> Self {
        Self { rate_bps: rate_bps.max(1) as f64, burst_bytes: burst_bytes as f64, tokens: burst_bytes as f64, last: None }
    }

    pub fn set_rate(&mut self, now: Micros, rate_bps: u64) {
        self.refill(now);
        self.rate_bps = rate_bps.max(1) as f64;
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps as u64
    }

    fn refill(&mut self, now: Micros) {
        if let Some(t) = self.last {
            let dt = now.saturating_sub(t) as f64 / 1e6;
            self.tokens = (self.tokens + dt * self.rate_bps / 8.0).min(self.burst_bytes);
        }
        self.last = Some(now);
    }

    /// Instant à partir duquel `len` octets pourront partir (sans rien consommer).
    pub fn ready_at(&self, now: Micros, len: usize) -> Micros {
        let tokens = match self.last {
            Some(t) => (self.tokens + now.saturating_sub(t) as f64 / 1e6 * self.rate_bps / 8.0).min(self.burst_bytes),
            None => self.tokens,
        };
        let need = len as f64;
        if tokens >= need || tokens >= self.burst_bytes {
            now
        } else {
            now + ((need - tokens) * 8.0 / self.rate_bps * 1e6).ceil() as Micros
        }
    }

    /// Tente de consommer `len` octets. Sinon retourne l'instant où ce sera possible.
    pub fn try_send(&mut self, now: Micros, len: usize) -> Result<(), Micros> {
        self.refill(now);
        let need = len as f64;
        if self.tokens >= need || self.tokens >= self.burst_bytes {
            self.tokens -= need;
            Ok(())
        } else {
            let wait = (need - self.tokens) * 8.0 / self.rate_bps * 1e6;
            Err(now + wait.ceil() as Micros)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_average_rate_and_burst() {
        // 12 Mb/s = 1500 octets/ms ; rafale max 3000 octets.
        let mut p = Pacer::new(12_000_000, 3000);
        let mut now = 0u64;
        let mut sent = 0usize;
        let mut max_burst = 0usize;
        let mut burst = 0usize;
        let mut last_t = 0;
        while now < 1_000_000 {
            match p.try_send(now, 1200) {
                Ok(()) => {
                    sent += 1200;
                    if now == last_t {
                        burst += 1200;
                    } else {
                        burst = 1200;
                    }
                    max_burst = max_burst.max(burst);
                    last_t = now;
                }
                Err(t) => {
                    assert!(t > now);
                    now = t;
                }
            }
        }
        let mbps = sent as f64 * 8.0 / 1e6;
        assert!((11.5..=12.5).contains(&mbps), "{mbps}");
        assert!(max_burst <= 3600, "{max_burst}");
    }

    #[test]
    fn idle_does_not_accumulate_unbounded_credit() {
        let mut p = Pacer::new(8_000_000, 2400);
        assert!(p.try_send(0, 1200).is_ok());
        // 10 s d'inactivité : seulement la rafale max est disponible.
        let mut n = 0;
        while p.try_send(10_000_000, 1200).is_ok() {
            n += 1;
            assert!(n < 10);
        }
        assert_eq!(n, 2);
    }
}
