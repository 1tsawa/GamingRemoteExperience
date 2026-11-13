//! Canal fiable et ordonné (Input, Control) : ARQ avec ACK cumulatif + SACK (64 bits),
//! retransmission sur délai (RTO) et retransmission rapide sur trous SACK.
//!
//! Un canal = un flux de messages numérotés `seq: u16`. Chaque message tient dans un paquet
//! (pas de fragmentation : un « touche relâchée » est minuscule). Le récepteur livre les
//! messages dans l'ordre, sans doublon.

use std::collections::VecDeque;

use crate::wire::{b0, seq16_lt, K_ACK, K_RELIABLE, ACK_LEN, RELIABLE_HDR};
use crate::Micros;

struct Entry {
    seq: u16,
    pkt: Vec<u8>,
    last_tx: Micros,
    tries: u32,
    acked: bool,
    skipped: u32,
    fast: bool,
}

/// Facteurs (en demi-unités) du RTO selon le nombre d'envois : 1, 1, 1.5, 2, 3, 4...
fn backoff_halves(tries: u32) -> u64 {
    const T: [u64; 6] = [2, 2, 3, 4, 6, 8];
    T[(tries.saturating_sub(1) as usize).min(T.len() - 1)]
}

pub struct ReliableTx {
    chan_flag: u8,
    next_seq: u16,
    inflight: VecDeque<Entry>,
    backlog: VecDeque<Vec<u8>>,
    window: usize,
    max_backlog: usize,
    pub retransmits: u64,
    pub sent: u64,
}

impl ReliableTx {
    pub fn new(chan_flag: u8, window: usize, max_backlog: usize) -> Self {
        Self {
            chan_flag,
            next_seq: 0,
            inflight: VecDeque::new(),
            backlog: VecDeque::new(),
            window: window.clamp(1, 4096),
            max_backlog,
            retransmits: 0,
            sent: 0,
        }
    }

    pub fn unacked(&self) -> usize {
        self.inflight.len() + self.backlog.len()
    }

    /// Met un message en file. `false` si le pair n'acquitte plus et que la file est pleine.
    pub fn push(&mut self, msg: &[u8]) -> bool {
        if self.inflight.len() < self.window && self.backlog.is_empty() {
            self.admit(msg.to_vec());
            true
        } else if self.backlog.len() < self.max_backlog {
            self.backlog.push_back(msg.to_vec());
            true
        } else {
            false
        }
    }

    fn admit(&mut self, msg: Vec<u8>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let mut pkt = Vec::with_capacity(RELIABLE_HDR + msg.len());
        pkt.push(b0(K_RELIABLE, self.chan_flag));
        pkt.extend_from_slice(&seq.to_le_bytes());
        pkt.extend_from_slice(&msg);
        self.inflight.push_back(Entry { seq, pkt, last_tx: 0, tries: 0, acked: false, skipped: 0, fast: false });
    }

    fn refill(&mut self) {
        while self.inflight.len() < self.window {
            match self.backlog.pop_front() {
                Some(m) => self.admit(m),
                None => break,
            }
        }
    }

    /// Prochain paquet à (ré)émettre.
    pub fn poll(&mut self, now: Micros, rto: Micros) -> Option<Vec<u8>> {
        for e in self.inflight.iter_mut() {
            if e.acked {
                continue;
            }
            let due = e.tries == 0 || e.fast || now.saturating_sub(e.last_tx) >= rto * backoff_halves(e.tries) / 2;
            if due {
                if e.tries > 0 {
                    self.retransmits += 1;
                }
                e.tries += 1;
                e.last_tx = now;
                e.fast = false;
                e.skipped = 0;
                self.sent += 1;
                return Some(e.pkt.clone());
            }
        }
        None
    }

    /// Instant de la prochaine action (0 = tout de suite).
    pub fn next_deadline(&self, rto: Micros) -> Option<Micros> {
        let mut best: Option<Micros> = None;
        for e in self.inflight.iter().filter(|e| !e.acked) {
            let t = if e.tries == 0 || e.fast { 0 } else { e.last_tx + rto * backoff_halves(e.tries) / 2 };
            best = Some(best.map_or(t, |b| b.min(t)));
        }
        best
    }

    /// Traite un ACK. `rtt_sample` reçoit les échantillons RTT valides (algorithme de Karn).
    pub fn on_ack(&mut self, now: Micros, cum: u16, sack: u64, min_fast_gap: Micros, mut rtt_sample: impl FnMut(Micros)) {
        // Un ACK ne peut pas dépasser ce qui a été émis (paquet forgé / très ancien).
        if seq16_lt(self.next_seq, cum) {
            return;
        }
        while let Some(f) = self.inflight.front() {
            if seq16_lt(f.seq, cum) {
                if f.tries == 1 && !f.acked {
                    rtt_sample(now.saturating_sub(f.last_tx));
                }
                self.inflight.pop_front();
            } else {
                break;
            }
        }
        let mut highest: Option<u16> = None;
        if let Some(front) = self.inflight.front().map(|f| f.seq) {
            for i in 0..64u16 {
                if sack >> i & 1 == 0 {
                    continue;
                }
                let seq = cum.wrapping_add(1).wrapping_add(i);
                let idx = seq.wrapping_sub(front) as usize;
                if let Some(e) = self.inflight.get_mut(idx) {
                    if e.seq == seq && !e.acked {
                        if e.tries == 1 {
                            rtt_sample(now.saturating_sub(e.last_tx));
                        }
                        e.acked = true;
                    }
                    if highest.is_none_or(|h| seq16_lt(h, seq)) {
                        highest = Some(seq);
                    }
                }
            }
        }
        if let Some(h) = highest {
            for e in self.inflight.iter_mut() {
                if !e.acked && e.tries > 0 && seq16_lt(e.seq, h) {
                    e.skipped += 1;
                    if e.skipped >= 2 && now.saturating_sub(e.last_tx) >= min_fast_gap {
                        e.fast = true;
                    }
                }
            }
        }
        self.refill();
    }
}

pub struct ReliableRx {
    next: u16,
    window: usize,
    buf: VecDeque<Option<Vec<u8>>>, // buf[i] <-> seq next + i
    pub duplicates: u64,
    pub delivered: u64,
}

impl ReliableRx {
    pub fn new(window: usize) -> Self {
        Self { next: 0, window: window.clamp(1, 4096), buf: VecDeque::new(), duplicates: 0, delivered: 0 }
    }

    /// Enregistre un message ; les messages désormais livrables dans l'ordre sont passés à `out`.
    pub fn on_message(&mut self, seq: u16, msg: &[u8], mut out: impl FnMut(Vec<u8>)) {
        let d = seq.wrapping_sub(self.next);
        if d >= 0x8000 {
            self.duplicates += 1; // déjà livré
            return;
        }
        let d = d as usize;
        if d >= self.window {
            return; // trop en avance : l'émetteur retransmettra
        }
        while self.buf.len() <= d {
            self.buf.push_back(None);
        }
        if self.buf[d].is_some() {
            self.duplicates += 1;
            return;
        }
        self.buf[d] = Some(msg.to_vec());
        while let Some(Some(_)) = self.buf.front() {
            let m = self.buf.pop_front().unwrap().unwrap();
            self.next = self.next.wrapping_add(1);
            self.delivered += 1;
            out(m);
        }
    }

    /// Paquet ACK : `[b0 | cum | sack]`.
    pub fn build_ack(&self, chan_flag: u8) -> Vec<u8> {
        let mut sack = 0u64;
        for i in 0..64usize {
            if matches!(self.buf.get(i + 1), Some(Some(_))) {
                sack |= 1 << i;
            }
        }
        let mut v = Vec::with_capacity(ACK_LEN);
        v.push(b0(K_ACK, chan_flag));
        v.extend_from_slice(&self.next.to_le_bytes());
        v.extend_from_slice(&sack.to_le_bytes());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_delivery_with_reordering_and_dups() {
        let mut rx = ReliableRx::new(256);
        let mut got = Vec::new();
        for &s in &[2u16, 0, 0, 3, 1, 1, 4] {
            rx.on_message(s, &[s as u8], |m| got.push(m[0]));
        }
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
        assert_eq!(rx.duplicates, 2);
    }

    #[test]
    fn sack_bitmap_reports_holes() {
        let mut rx = ReliableRx::new(256);
        rx.on_message(0, b"a", |_| {});
        rx.on_message(2, b"c", |_| {});
        rx.on_message(3, b"d", |_| {});
        let ack = rx.build_ack(0);
        let cum = u16::from_le_bytes([ack[1], ack[2]]);
        let sack = u64::from_le_bytes(ack[3..11].try_into().unwrap());
        assert_eq!(cum, 1); // 1 manque
        assert_eq!(sack, 0b11); // 2 et 3 reçus
    }

    #[test]
    fn tx_retransmits_until_acked() {
        let mut tx = ReliableTx::new(0, 64, 64);
        for i in 0..3u8 {
            assert!(tx.push(&[i]));
        }
        let mut now = 0;
        let mut sent = Vec::new();
        while let Some(p) = tx.poll(now, 50_000) {
            sent.push(p);
        }
        assert_eq!(sent.len(), 3);
        assert!(tx.poll(10_000, 50_000).is_none());
        // RTO écoulé : tout est réémis
        now = 60_000;
        let mut n = 0;
        while tx.poll(now, 50_000).is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
        // ACK cumulatif de tout
        tx.on_ack(70_000, 3, 0, 0, |_| {});
        assert_eq!(tx.unacked(), 0);
        assert!(tx.poll(1_000_000, 50_000).is_none());
    }

    #[test]
    fn sack_triggers_fast_retransmit_and_window_refills() {
        let mut tx = ReliableTx::new(0, 4, 100);
        for i in 0..10u8 {
            assert!(tx.push(&[i]));
        }
        assert_eq!(tx.inflight.len(), 4);
        while tx.poll(0, 1_000_000).is_some() {}
        // seq 0 perdu ; 1,2,3 acquittés : deux ACK successifs déclenchent la retransmission de 0
        tx.on_ack(10_000, 0, 0b110, 0, |_| {});
        tx.on_ack(11_000, 0, 0b110, 0, |_| {});
        let p = tx.poll(12_000, 1_000_000).expect("fast retransmit");
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0);
        tx.on_ack(20_000, 4, 0, 0, |_| {});
        assert_eq!(tx.inflight.len(), 4); // rempli depuis le backlog
    }

    #[test]
    fn forged_ack_is_ignored() {
        let mut tx = ReliableTx::new(0, 8, 8);
        tx.push(b"x");
        tx.poll(0, 1000);
        tx.on_ack(1, 5000, u64::MAX, 0, |_| {});
        assert_eq!(tx.unacked(), 1);
    }

    #[test]
    fn backlog_is_bounded() {
        let mut tx = ReliableTx::new(0, 2, 3);
        let ok = (0..10).filter(|_| tx.push(b"m")).count();
        assert_eq!(ok, 5);
    }
}
