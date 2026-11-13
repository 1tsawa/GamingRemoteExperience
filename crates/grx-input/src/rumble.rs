//! Module de gestion des messages de vibration (rumble).
//!
//! Format binaire strict de 10 octets :
//! `[magie 7 octets][index u8][large u8][small u8]`

use crate::event::MAX_GAMEPADS;

pub const RUMBLE_MAGIC: &[u8; 7] = b"GRX-RMB";
pub const RUMBLE_LEN: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rumble {
    pub index: u8,
    /// gros moteur (basse frequence, gauche) 0..=255
    pub large: u8,
    /// petit moteur (haute frequence, droite)
    pub small: u8,
}

impl Rumble {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(RUMBLE_LEN);
        buf.extend_from_slice(RUMBLE_MAGIC);
        buf.push(self.index);
        buf.push(self.large);
        buf.push(self.small);
        buf
    }

    pub fn decode(b: &[u8]) -> Option<Rumble> {
        if b.len() != RUMBLE_LEN {
            return None;
        }
        if !b.starts_with(RUMBLE_MAGIC) {
            return None;
        }
        let index = b[7];
        if index as usize >= MAX_GAMEPADS {
            return None;
        }
        Some(Rumble { index, large: b[8], small: b[9] })
    }

    pub fn is_off(&self) -> bool {
        self.large == 0 && self.small == 0
    }
}

/// Vrai si le message commence par la magie (pour l'aiguillage), meme s'il est invalide.
pub fn is_rumble(b: &[u8]) -> bool {
    b.len() >= RUMBLE_MAGIC.len() && b.starts_with(RUMBLE_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_combinations() {
        for idx in [0u8, 3] {
            for l in [0u8, 255] {
                for s in [0u8, 255] {
                    let r = Rumble { index: idx, large: l, small: s };
                    let bytes = r.encode();
                    assert_eq!(bytes.len(), RUMBLE_LEN);
                    assert_eq!(Rumble::decode(&bytes), Some(r));
                }
            }
        }
    }

    #[test]
    fn known_wire_bytes() {
        let r = Rumble { index: 1, large: 2, small: 3 };
        let expected = [b'G', b'R', b'X', b'-', b'R', b'M', b'B', 1, 2, 3];
        assert_eq!(r.encode(), expected);
        assert_eq!(Rumble::decode(&expected), Some(r));
    }

    #[test]
    fn rejects_malformed() {
        assert!(Rumble::decode(&[]).is_none());
        assert!(Rumble::decode(&[0; 9]).is_none());
        assert!(Rumble::decode(&[0; 11]).is_none());
        assert!(Rumble::decode(&[b'X', b'R', b'X', b'-', b'R', b'M', b'B', 0, 0, 0]).is_none());
        assert!(Rumble::decode(&[b'G', b'R', b'X', b'-', b'R', b'M', b'B', 4, 0, 0]).is_none());
        assert!(Rumble::decode(&[b'G', b'R', b'X', b'-', b'R', b'M', b'B', 255, 0, 0]).is_none());
    }

    #[test]
    fn is_rumble_checks() {
        assert!(is_rumble(RUMBLE_MAGIC));
        assert!(is_rumble(&[b'G', b'R', b'X', b'-', b'R', b'M', b'B', 0, 0, 0]));
        assert!(!is_rumble(&[]));
        assert!(!is_rumble(b"GRX-RM"));
        assert!(!is_rumble(b"XRX-RMB"));
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn fuzz_never_panics_and_valid_results_reencode_identically() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut accepted = 0u32;
        for _ in 0..200_000 {
            let len = (rng.next() % 16) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            // biaise le debut vers la magie complete pour atteindre le decodage des champs
            if !rng.next().is_multiple_of(4) {
                let n = buf.len().min(RUMBLE_MAGIC.len());
                buf[..n].copy_from_slice(&RUMBLE_MAGIC[..n]);
            }
            if let Some(r) = Rumble::decode(&buf) {
                accepted += 1;
                assert_eq!(r.encode(), buf, "decodage non canonique pour {buf:?}");
            }
        }
        assert!(accepted > 100, "le fuzz doit atteindre des cas valides ({accepted})");
    }
}
