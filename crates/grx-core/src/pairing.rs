//! Première connexion sans lien d'invitation (« demande d'accès ») et test de joignabilité.
//!
//! Protocole (indépendant de la plateforme, voir `docs/ARCHITECTURE.md`, « Demandes d'accès ») :
//!
//! ```text
//! client -> host : KEY_REQ  = [0x03]["GRXK"][version u16 LE][sonde 16 octets][horodatage u64 LE][zéros] (64 octets)
//! host -> client : KEY_RESP = [0x04]["GRXK"][version u16 LE][clé publique du host 32 octets] (39 octets)
//! puis           : INIT Noise IK habituel, avec `pair: true` dans la charge utile ; le host répond
//!                  un RESP chiffré dont la charge porte `pair: pending|denied|accepted` (sid 0,
//!                  AUCUNE session créée). Le client recommence toutes les ~2 s jusqu'à la décision.
//!                  L'INIT de demande est rembourré (champ `pad`) : la réponse chiffrée n'est
//!                  jamais plus grosse que lui (le host se tait sinon).
//! ```
//!
//! Le host ne répond à KEY_REQ que si les demandes sont ouvertes (`trust.json`,
//! `requests_open_until_ms`) OU si la sonde est la sienne : `sonde = SHA-256(clé de sonde ‖
//! horodatage)[..16]`, la clé de sonde étant dérivée de la clé privée du host (seul le propriétaire
//! du host peut tester sa propre joignabilité, les inconnus n'obtiennent rien). Une sonde n'est
//! valable que ±30 s et une seule fois (horodatage strictement croissant) : une sonde capturée ne
//! se rejoue pas. La réponse est plus petite que la requête : aucune amplification.
//!
//! La clé du host obtenue ainsi n'est PAS authentifiée (confiance au premier usage) : les deux
//! côtés affichent les empreintes, à comparer pour exclure un intermédiaire.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::PublicKey;

pub const PKT_KEY_REQ: u8 = 0x03;
pub const PKT_KEY_RESP: u8 = 0x04;
const MAGIC: &[u8; 4] = b"GRXK";
pub const PAIR_VERSION: u16 = 1;
/// Taille exacte d'une KEY_REQ (rembourrée : plus grosse que la réponse).
pub const KEY_REQ_LEN: usize = 64;
pub const KEY_RESP_LEN: usize = 1 + 4 + 2 + 32;
pub const PROBE_LEN: usize = 16;
/// Validité d'une sonde autour de l'horloge du host (le test part du même PC).
pub const PROBE_SKEW_MS: u64 = 30_000;

/// Sonde de joignabilité envoyée dans une KEY_REQ.
pub type Probe = [u8; PROBE_LEN];

/// Clé de sonde : dérivée de la clé privée du host (jamais la clé elle-même), connue seulement
/// du host et de la GUI du même PC.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProbeKey([u8; 32]);

impl std::fmt::Debug for ProbeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProbeKey(..)")
    }
}

impl ProbeKey {
    pub(crate) fn from_secret(secret: &[u8; 32]) -> Self {
        let mut h = Sha256::new();
        h.update(b"GRX-PROBE-KEY-v1");
        h.update(secret);
        Self(h.finalize().into())
    }

    /// Sonde valable pour l'horodatage `ts_ms`.
    pub fn probe(&self, ts_ms: u64) -> Probe {
        let mut h = Sha256::new();
        h.update(b"GRX-PROBE-v2");
        h.update(self.0);
        h.update(ts_ms.to_le_bytes());
        h.finalize()[..PROBE_LEN].try_into().expect("16 octets")
    }

    /// Vérifie une sonde reçue : bonne valeur (temps constant), horodatage à ±30 s de `now_ms` et
    /// strictement plus récent que `*last_ts` (anti-rejeu ; mis à jour si acceptée).
    pub fn check(&self, got: &Probe, ts_ms: u64, now_ms: u64, last_ts: &mut u64) -> bool {
        if ts_ms.abs_diff(now_ms) > PROBE_SKEW_MS || ts_ms <= *last_ts {
            return false;
        }
        if !probe_matches(got, &self.probe(ts_ms)) {
            return false;
        }
        *last_ts = ts_ms;
        true
    }
}

/// KEY_REQ portant une sonde fraîche du host (horodatage strictement croissant dans ce processus).
pub fn probe_request(key: &ProbeKey, now_ms: u64) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let prev = LAST.fetch_max(now_ms, Ordering::SeqCst);
    let ts = if prev >= now_ms { LAST.fetch_add(1, Ordering::SeqCst) + 1 } else { now_ms };
    key_request(Some((&key.probe(ts), ts)))
}

/// État d'une demande d'accès, renvoyé (chiffré) par le host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairStatus {
    /// En attente de validation sur le host.
    Pending,
    /// Refusée par le host.
    Denied,
    /// Acceptée : la clé du client est autorisée, une connexion normale marchera.
    Accepted,
}

/// Requête de clé ; `probe` = (sonde, horodatage) du host (test de joignabilité par son propriétaire).
pub fn key_request(probe: Option<(&Probe, u64)>) -> Vec<u8> {
    let mut p = vec![0u8; KEY_REQ_LEN];
    p[0] = PKT_KEY_REQ;
    p[1..5].copy_from_slice(MAGIC);
    p[5..7].copy_from_slice(&PAIR_VERSION.to_le_bytes());
    if let Some((pr, ts)) = probe {
        p[7..7 + PROBE_LEN].copy_from_slice(pr);
        p[7 + PROBE_LEN..15 + PROBE_LEN].copy_from_slice(&ts.to_le_bytes());
    }
    p
}

/// Analyse une KEY_REQ : `Some((sonde, horodatage))` (tout à zéro = sans sonde) si le paquet est
/// bien formé.
pub fn parse_key_request(pkt: &[u8]) -> Option<(Probe, u64)> {
    if pkt.len() != KEY_REQ_LEN || pkt[0] != PKT_KEY_REQ || &pkt[1..5] != MAGIC {
        return None;
    }
    if u16::from_le_bytes([pkt[5], pkt[6]]) != PAIR_VERSION {
        return None;
    }
    let probe = pkt[7..7 + PROBE_LEN].try_into().ok()?;
    let ts = u64::from_le_bytes(pkt[7 + PROBE_LEN..15 + PROBE_LEN].try_into().ok()?);
    Some((probe, ts))
}

/// Comparaison en temps constant d'une sonde reçue avec celle du host (une sonde nulle ne
/// correspond jamais).
pub fn probe_matches(got: &Probe, mine: &Probe) -> bool {
    let diff = got.iter().zip(mine).fold(0u8, |a, (x, y)| a | (x ^ y));
    let zero = got.iter().fold(0u8, |a, x| a | x);
    diff == 0 && zero != 0
}

pub fn key_response(host_key: &PublicKey) -> Vec<u8> {
    let mut p = Vec::with_capacity(KEY_RESP_LEN);
    p.push(PKT_KEY_RESP);
    p.extend_from_slice(MAGIC);
    p.extend_from_slice(&PAIR_VERSION.to_le_bytes());
    p.extend_from_slice(host_key);
    p
}

pub fn parse_key_response(pkt: &[u8]) -> Option<PublicKey> {
    if pkt.len() != KEY_RESP_LEN || pkt[0] != PKT_KEY_RESP || &pkt[1..5] != MAGIC {
        return None;
    }
    if u16::from_le_bytes([pkt[5], pkt[6]]) != PAIR_VERSION {
        return None;
    }
    pkt[7..].try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_request_roundtrip_and_no_amplification() {
        let r = key_request(None);
        assert_eq!(r.len(), KEY_REQ_LEN);
        assert_eq!(parse_key_request(&r), Some(([0u8; 16], 0)));
        let pr = [7u8; 16];
        assert_eq!(parse_key_request(&key_request(Some((&pr, 42)))), Some((pr, 42)));
        const { assert!(KEY_RESP_LEN < KEY_REQ_LEN, "la réponse doit être plus petite que la requête") };
        // Mauvaise taille, type, magie ou version : ignoré.
        assert!(parse_key_request(&r[..63]).is_none());
        let mut bad = r.clone();
        bad[1] = b'X';
        assert!(parse_key_request(&bad).is_none());
        let mut bad = r.clone();
        bad[5] = 9;
        assert!(parse_key_request(&bad).is_none());
        let mut bad = r;
        bad[0] = PKT_KEY_RESP;
        assert!(parse_key_request(&bad).is_none());
    }

    #[test]
    fn key_response_roundtrip() {
        let k = [42u8; 32];
        let p = key_response(&k);
        assert_eq!(p.len(), KEY_RESP_LEN);
        assert_eq!(parse_key_response(&p), Some(k));
        assert!(parse_key_response(&p[..38]).is_none());
        let mut bad = p;
        bad[0] = PKT_KEY_REQ;
        assert!(parse_key_response(&bad).is_none());
    }

    #[test]
    fn probe_matching() {
        let k = ProbeKey::from_secret(&[1u8; 32]);
        let mine = k.probe(1000);
        assert!(probe_matches(&mine, &mine));
        assert!(!probe_matches(&[0u8; 16], &[0u8; 16]), "sonde nulle jamais acceptée");
        let mut other = mine;
        other[15] ^= 1;
        assert!(!probe_matches(&other, &mine));
        assert_ne!(ProbeKey::from_secret(&[2u8; 32]).probe(1000), mine);
        assert_ne!(k.probe(1001), mine, "la sonde dépend de l'horodatage");
    }

    #[test]
    fn probe_check_freshness_and_replay() {
        let k = ProbeKey::from_secret(&[1u8; 32]);
        let now = 1_700_000_000_000u64;
        let mut last = 0;
        assert!(k.check(&k.probe(now), now, now + 100, &mut last));
        assert_eq!(last, now);
        // Rejeu exact : refusé.
        assert!(!k.check(&k.probe(now), now, now + 200, &mut last));
        // Trop vieille ou trop future : refusée.
        assert!(!k.check(&k.probe(now - 60_000), now - 60_000, now, &mut last));
        assert!(!k.check(&k.probe(now + 60_000), now + 60_000, now, &mut last));
        // Mauvaise clé : refusée.
        let other = ProbeKey::from_secret(&[3u8; 32]);
        assert!(!k.check(&other.probe(now + 1), now + 1, now, &mut last));
        assert!(k.check(&k.probe(now + 1), now + 1, now, &mut last));
        // Les requêtes générées sont toujours fraîches (horodatage croissant, même à la même ms).
        let (p1, t1) = parse_key_request(&probe_request(&k, now + 10)).unwrap();
        let (p2, t2) = parse_key_request(&probe_request(&k, now + 10)).unwrap();
        assert!(t2 > t1);
        assert!(k.check(&p1, t1, now, &mut last) && k.check(&p2, t2, now, &mut last));
    }

    #[test]
    fn status_json_names() {
        assert_eq!(serde_json::to_string(&PairStatus::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::from_str::<PairStatus>("\"accepted\"").unwrap(), PairStatus::Accepted);
    }
}
