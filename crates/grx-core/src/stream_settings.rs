//! Réglages de session à chaud, transportés comme octets opaques dans le canal Control fiable.
//!
//! Format (petit-boutiste) : `[magie 7 o][version u8 = 1][count u8][count x (clé u8, valeur u32)]`,
//! longueur exacte `9 + 5*count`, `count <= MAX_ENTRIES`. Tout autre message est refusé (`None`),
//! une clé inconnue est ignorée, une clé répétée : la dernière gagne.
//!
//! * `GRX-SET` (client -> host) : [`StreamSettings`] (1 = plafond de débit kb/s, 2 = images/s max ;
//!   0 = pas de plafond). Les valeurs sont bornées au décodage.
//! * `GRX-HST` (host -> client) : [`HostInfo`] (plafonds appliqués + limites du host), envoyé au
//!   début de la session et après chaque réglage.

pub const SETTINGS_MAGIC: &[u8; 7] = b"GRX-SET";
pub const HOST_INFO_MAGIC: &[u8; 7] = b"GRX-HST";
pub const VERSION: u8 = 1;
pub const MAX_ENTRIES: usize = 8;

pub const MIN_BITRATE_CAP_KBPS: u32 = 1_000;
pub const MAX_BITRATE_CAP_KBPS: u32 = 1_000_000;
pub const MIN_FPS_CAP: u32 = 10;
pub const MAX_FPS_CAP: u32 = 240;

const HEADER: usize = 9;
const ENTRY: usize = 5;

/// Réglages demandés par le client. `None` = pas de plafond (valeur 0 sur le fil).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamSettings {
    pub max_bitrate_kbps: Option<u32>,
    pub max_fps: Option<u32>,
}

/// État annoncé par le host (début de session et après chaque réglage appliqué).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostInfo {
    /// Plafond de débit appliqué (kb/s), `None` = aucun.
    pub bitrate_cap_kbps: Option<u32>,
    /// Plafond d'images/s appliqué, `None` = aucun.
    pub fps_cap: Option<u32>,
    /// Débit maximal configuré côté host (kb/s, FEC comprise).
    pub host_max_bitrate_kbps: u32,
    /// Cadence maximale configurée côté host (`--fps`).
    pub host_fps: u32,
}

fn opt(v: Option<u32>) -> u32 {
    v.unwrap_or(0)
}

fn nonzero(v: u32) -> Option<u32> {
    (v != 0).then_some(v)
}

fn encode_entries(magic: &[u8; 7], entries: &[(u8, u32)]) -> Vec<u8> {
    debug_assert!(entries.len() <= MAX_ENTRIES);
    let mut b = Vec::with_capacity(HEADER + ENTRY * entries.len());
    b.extend_from_slice(magic);
    b.push(VERSION);
    b.push(entries.len() as u8);
    for &(k, v) in entries {
        b.push(k);
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

fn decode_entries(magic: &[u8; 7], b: &[u8]) -> Option<Vec<(u8, u32)>> {
    if b.get(..7)? != magic || *b.get(7)? != VERSION {
        return None;
    }
    let count = *b.get(8)? as usize;
    if count > MAX_ENTRIES || b.len() != HEADER + ENTRY * count {
        return None;
    }
    let body = b.get(HEADER..)?;
    Some(body.as_chunks::<ENTRY>().0.iter().map(|e| (e[0], u32::from_le_bytes([e[1], e[2], e[3], e[4]]))).collect())
}

/// Le message est-il un réglage client (`GRX-SET`) ?
pub fn is_settings(b: &[u8]) -> bool {
    b.starts_with(SETTINGS_MAGIC)
}

/// Le message est-il une annonce du host (`GRX-HST`) ?
pub fn is_host_info(b: &[u8]) -> bool {
    b.starts_with(HOST_INFO_MAGIC)
}

impl StreamSettings {
    pub fn encode(&self) -> Vec<u8> {
        encode_entries(SETTINGS_MAGIC, &[(1, opt(self.max_bitrate_kbps)), (2, opt(self.max_fps))])
    }

    pub fn decode(b: &[u8]) -> Option<StreamSettings> {
        let mut s = StreamSettings::default();
        for (k, v) in decode_entries(SETTINGS_MAGIC, b)? {
            match k {
                1 => s.max_bitrate_kbps = nonzero(v).map(|v| v.clamp(MIN_BITRATE_CAP_KBPS, MAX_BITRATE_CAP_KBPS)),
                2 => s.max_fps = nonzero(v).map(|v| v.clamp(MIN_FPS_CAP, MAX_FPS_CAP)),
                _ => {}
            }
        }
        Some(s)
    }
}

impl HostInfo {
    pub fn encode(&self) -> Vec<u8> {
        encode_entries(
            HOST_INFO_MAGIC,
            &[
                (1, opt(self.bitrate_cap_kbps)),
                (2, opt(self.fps_cap)),
                (3, self.host_max_bitrate_kbps),
                (4, self.host_fps),
            ],
        )
    }

    pub fn decode(b: &[u8]) -> Option<HostInfo> {
        let mut h = HostInfo::default();
        for (k, v) in decode_entries(HOST_INFO_MAGIC, b)? {
            match k {
                1 => h.bitrate_cap_kbps = nonzero(v),
                2 => h.fps_cap = nonzero(v),
                3 => h.host_max_bitrate_kbps = v,
                4 => h.host_fps = v,
                _ => {}
            }
        }
        Some(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        for (b, f) in [(None, None), (Some(5_000), None), (None, Some(60)), (Some(30_000), Some(144))] {
            let s = StreamSettings { max_bitrate_kbps: b, max_fps: f };
            assert_eq!(StreamSettings::decode(&s.encode()), Some(s));
        }
        let h = HostInfo { bitrate_cap_kbps: Some(12_000), fps_cap: None, host_max_bitrate_kbps: 50_000, host_fps: 60 };
        assert_eq!(HostInfo::decode(&h.encode()), Some(h));
        assert_eq!(HostInfo::decode(&HostInfo::default().encode()), Some(HostInfo::default()));
    }

    #[test]
    fn values_are_clamped() {
        let s = StreamSettings { max_bitrate_kbps: Some(1), max_fps: Some(1) };
        let d = StreamSettings::decode(&s.encode()).unwrap();
        assert_eq!(d, StreamSettings { max_bitrate_kbps: Some(1_000), max_fps: Some(10) });
        let s = StreamSettings { max_bitrate_kbps: Some(5_000_000), max_fps: Some(1000) };
        let d = StreamSettings::decode(&s.encode()).unwrap();
        assert_eq!(d, StreamSettings { max_bitrate_kbps: Some(1_000_000), max_fps: Some(240) });
    }

    #[test]
    fn malformed_messages_are_refused() {
        let good = StreamSettings { max_bitrate_kbps: Some(8_000), max_fps: Some(60) }.encode();
        assert_eq!(StreamSettings::decode(&[]), None);
        for n in 0..good.len() {
            assert_eq!(StreamSettings::decode(&good[..n]), None, "préfixe {n}");
        }
        let mut extra = good.clone();
        extra.push(0);
        assert_eq!(StreamSettings::decode(&extra), None);
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert_eq!(StreamSettings::decode(&bad_magic), None);
        let mut v2 = good.clone();
        v2[7] = 2;
        assert_eq!(StreamSettings::decode(&v2), None);
        // count = 9 (> MAX_ENTRIES) avec la longueur qui irait avec.
        let mut too_many = good[..9].to_vec();
        too_many[8] = 9;
        too_many.extend(std::iter::repeat_n(0u8, 9 * 5));
        assert_eq!(StreamSettings::decode(&too_many), None);
    }

    #[test]
    fn kinds_are_not_confused() {
        let s = StreamSettings { max_bitrate_kbps: Some(8_000), max_fps: None }.encode();
        let h = HostInfo::default().encode();
        assert_eq!(HostInfo::decode(&s), None);
        assert_eq!(StreamSettings::decode(&h), None);
        assert!(is_settings(&s) && !is_host_info(&s));
        assert!(is_host_info(&h) && !is_settings(&h));
        assert!(!is_settings(b"GRX-BYE") && !is_host_info(b"GRX-BYE"));
        assert_eq!(StreamSettings::decode(b"GRX-BYE"), None);
    }

    #[test]
    fn unknown_keys_ignored_and_last_duplicate_wins() {
        let b = encode_entries(SETTINGS_MAGIC, &[(1, 2_000), (99, 7), (1, 3_000)]);
        assert_eq!(StreamSettings::decode(&b), Some(StreamSettings { max_bitrate_kbps: Some(3_000), max_fps: None }));
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
        for i in 0..200_000 {
            let len = (next() % 65) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if i % 2 == 0 && b.len() >= 8 {
                let m = if i % 4 == 0 { SETTINGS_MAGIC } else { HOST_INFO_MAGIC };
                b[..7].copy_from_slice(m);
                b[7] = VERSION;
                if b.len() >= 9 && i % 8 == 0 {
                    // Longueur cohérente pour atteindre le décodage des entrées.
                    let c = (next() % 9) as usize;
                    b.truncate(9);
                    b[8] = c as u8;
                    b.extend((0..c * 5).map(|_| next() as u8));
                }
            }
            if let Some(s) = StreamSettings::decode(&b) {
                accepted += 1;
                assert!(s.max_bitrate_kbps.is_none_or(|v| (MIN_BITRATE_CAP_KBPS..=MAX_BITRATE_CAP_KBPS).contains(&v)));
                assert!(s.max_fps.is_none_or(|v| (MIN_FPS_CAP..=MAX_FPS_CAP).contains(&v)));
            }
            let _ = HostInfo::decode(&b);
        }
        assert!(accepted > 1000, "le fuzz doit aussi atteindre des messages valides : {accepted}");
    }
}
