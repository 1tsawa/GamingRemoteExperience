//! Message Control host -> client `GRX-SEC` signalant un écran sécurisé Windows (UAC/verrouillage)
//! ou une fenêtre administrateur au premier plan côté host, et ce qui est bloqué pour ce client.
//!
//! Format binaire : `[magie 7 o][version u8 = 1][screen u8 0..=2][flags u8][reason u8 0..=2]`
//! Longueur exacte : 11 octets.
//! Décodage strict, jamais de panique. L'encodage respecte scrupuleusement le format.

pub const SECURE_NOTICE_MAGIC: &[u8; 7] = b"GRX-SEC";
pub const VERSION: u8 = 1;
pub const SECURE_NOTICE_LEN: usize = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum HostScreen {
    #[default]
    Normal,
    SecureDesktop,
    ElevatedWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum BlockReason {
    #[default]
    None,
    /// Rôle du client insuffisant (« Contrôle total » requis).
    Role,
    /// Le host ne peut pas agir (non élevé, ou bureau sécurisé : à valider sur place).
    HostMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct SecureNotice {
    pub screen: HostScreen,
    pub video_blocked: bool,
    pub input_blocked: bool,
    /// Le host tourne élevé (mode « Contrôle total »).
    pub host_elevated: bool,
    pub reason: BlockReason,
}

impl SecureNotice {
    /// Encode la notification au format binaire `GRX-SEC`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(SECURE_NOTICE_LEN);
        buf.extend_from_slice(SECURE_NOTICE_MAGIC);
        buf.push(VERSION);
        buf.push(self.screen as u8);
        let flags = (self.video_blocked as u8)
            | ((self.input_blocked as u8) << 1)
            | ((self.host_elevated as u8) << 2);
        buf.push(flags);
        buf.push(self.reason as u8);
        buf
    }

    /// Décodage strict du message `GRX-SEC`. Retourne `None` en cas de format invalide.
    pub fn decode(m: &[u8]) -> Option<Self> {
        if m.len() != SECURE_NOTICE_LEN {
            return None;
        }
        if m.get(..7)? != SECURE_NOTICE_MAGIC {
            return None;
        }
        if *m.get(7)? != VERSION {
            return None;
        }
        let screen_code = *m.get(8)?;
        if screen_code > 2 {
            return None;
        }
        let flags = *m.get(9)?;
        if flags & 0xF8 != 0 {
            return None;
        }
        let reason_code = *m.get(10)?;
        if reason_code > 2 {
            return None;
        }

        Some(SecureNotice {
            screen: match screen_code {
                0 => HostScreen::Normal,
                1 => HostScreen::SecureDesktop,
                2 => HostScreen::ElevatedWindow,
                _ => unreachable!(),
            },
            video_blocked: (flags & 1) != 0,
            input_blocked: (flags & 2) != 0,
            host_elevated: (flags & 4) != 0,
            reason: match reason_code {
                0 => BlockReason::None,
                1 => BlockReason::Role,
                2 => BlockReason::HostMode,
                _ => unreachable!(),
            },
        })
    }
}

/// Le message est-il une notification de sécurité (`GRX-SEC`) ?
pub fn is_secure_notice(m: &[u8]) -> bool {
    m.starts_with(SECURE_NOTICE_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_all_combinations() {
        let screens = [HostScreen::Normal, HostScreen::SecureDesktop, HostScreen::ElevatedWindow];
        let reasons = [BlockReason::None, BlockReason::Role, BlockReason::HostMode];
        let mut flags_combos = Vec::new();
        for v in 0..2 {
            for i in 0..2 {
                for t in 0..2 {
                    flags_combos.push((v != 0, i != 0, t != 0));
                }
            }
        }

        for screen in screens {
            for reason in reasons {
                for (video, input, total) in flags_combos.iter() {
                    let notice = SecureNotice {
                        screen,
                        video_blocked: *video,
                        input_blocked: *input,
                        host_elevated: *total,
                        reason,
                    };
                    let encoded = notice.encode();
                    assert_eq!(encoded.len(), SECURE_NOTICE_LEN);
                    assert_eq!(SecureNotice::decode(&encoded), Some(notice));
                }
            }
        }
    }

    #[test]
    fn rejections() {
        let valid = SecureNotice::default().encode();
        assert_eq!(SecureNotice::decode(&[]), None);
        assert_eq!(SecureNotice::decode(SECURE_NOTICE_MAGIC), None);
        assert_eq!(SecureNotice::decode(&valid[..10]), None);
        let mut extra = valid.clone();
        extra.push(0);
        assert_eq!(SecureNotice::decode(&extra), None);

        let mut v0 = valid.clone();
        v0[7] = 0;
        assert_eq!(SecureNotice::decode(&v0), None);

        let mut v2 = valid.clone();
        v2[7] = 2;
        assert_eq!(SecureNotice::decode(&v2), None);

        let mut s3 = valid.clone();
        s3[8] = 3;
        assert_eq!(SecureNotice::decode(&s3), None);

        let mut r3 = valid.clone();
        r3[10] = 3;
        assert_eq!(SecureNotice::decode(&r3), None);

        for bit in 3..8 {
            let mut bad_flags = valid.clone();
            bad_flags[9] |= 1 << bit;
            assert_eq!(SecureNotice::decode(&bad_flags), None, "bit réservé {}", bit);
        }
    }

    #[test]
    fn is_secure_notice_tests() {
        assert!(is_secure_notice(SECURE_NOTICE_MAGIC));
        assert!(!is_secure_notice(b"GRX-SET"));
        assert!(is_secure_notice(&SecureNotice::default().encode()));
    }

    #[test]
    fn fuzz_never_panics_and_respects_bounds() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };

        let mut accepted = 0;
        // 200 000 tampons aléatoires de longueur 0..32
        for i in 0..200_000 {
            let len = (next() % 32) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if i % 2 == 0 && b.len() >= 7 {
                b[..7].copy_from_slice(SECURE_NOTICE_MAGIC);
            }
            if let Some(decoded) = SecureNotice::decode(&b) {
                accepted += 1;
                assert_eq!(SecureNotice::decode(&decoded.encode()), Some(decoded));
            }
        }

        // 50 000 mutations d'un message valide (un octet modifié)
        let valid = SecureNotice::default().encode();
        for _ in 0..50_000 {
            let mut b = valid.clone();
            let pos = (next() as usize) % b.len();
            b[pos] = next() as u8;
            if let Some(decoded) = SecureNotice::decode(&b) {
                accepted += 1;
                assert_eq!(SecureNotice::decode(&decoded.encode()), Some(decoded));
            }
        }

        assert!(accepted >= 100, "au moins quelques messages acceptés : {}", accepted);
    }
}
