//! Poignée de main + transport sur UDP.
//!
//! Noise IK (1 aller-retour) :
//!   client -> host : INIT  = [0x01][e, es, s, ss + payload chiffré]
//!   host -> client : RESP  = [0x02][e, ee, se + payload chiffré]
//!   ensuite        : DATA  = [0x10][session id u32][nonce u64][AES-256-GCM(ct + tag)]
//!
//! Le client doit connaître la clé du host (épinglée dans l'invitation). Le host ne répond
//! QU'AUX clients autorisés ou porteurs d'un jeton valide : un inconnu n'obtient aucun octet.

use std::collections::HashMap;

use rand::RngCore;
use zeroize::Zeroize;
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, StatelessTransportState};

use crate::identity::{Identity, PublicKey, NOISE_PARAMS};
use crate::invite::Token;
use crate::trust::TrustStore;
use crate::{Error, Result};

pub const PKT_INIT: u8 = 0x01;
pub const PKT_RESP: u8 = 0x02;
pub const PKT_DATA: u8 = 0x10;

pub const PROTO_VERSION: u16 = 1;
const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
const DATA_HEADER: usize = 1 + 4 + 8;
const TAG_LEN: usize = 16;
const MAX_NAME_LEN: usize = 64;

/// Taille minimale réelle d'un INIT : 1 (type) + 32 (e) + 48 (s chiffrée) + 16 (tag) + le plus petit
/// JSON possible `{"v":1,"ts_ms":0,"name":"","token":null}` (40 octets) + 16 (tag).
pub const MIN_INIT_LEN: usize = 1 + 32 + 48 + 16 + 40;
/// Taille maximale acceptée d'un INIT (nom 64 caractères + jeton : bien en dessous).
pub const MAX_INIT_LEN: usize = 1024;

#[derive(Serialize, Deserialize)]
struct InitPayload {
    v: u16,
    ts_ms: u64,
    name: String,
    token: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RespPayload {
    v: u16,
    sid: u32,
    name: String,
}

fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_NAME_LEN).collect()
}

/// Fenêtre anti-rejeu glissante (1024 paquets), tolérante au réordonnancement UDP.
pub struct ReplayWindow {
    top: u64,
    started: bool,
    seen: Vec<bool>,
}

const W: u64 = 1024;

impl ReplayWindow {
    pub fn new() -> Self {
        Self { top: 0, started: false, seen: vec![false; W as usize] }
    }

    pub fn check(&self, n: u64) -> bool {
        if !self.started || n > self.top {
            return true;
        }
        self.top - n < W && !self.seen[(n % W) as usize]
    }

    pub fn update(&mut self, n: u64) {
        if !self.started {
            self.started = true;
            self.top = n;
            self.seen.fill(false);
        } else if n > self.top {
            let d = n - self.top;
            if d >= W {
                self.seen.fill(false);
            } else {
                for i in 1..=d {
                    self.seen[((self.top + i) % W) as usize] = false;
                }
            }
            self.top = n;
        }
        self.seen[(n % W) as usize] = true;
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// Canal chiffré établi. Les deux sens ont des clés distinctes (propriété de Noise),
/// donc le même compteur peut être utilisé dans les deux directions.
pub struct Session {
    sid: u32,
    transport: StatelessTransportState,
    tx_ctr: u64,
    replay: ReplayWindow,
    peer: PublicKey,
    peer_name: String,
}

impl Session {
    pub fn sid(&self) -> u32 {
        self.sid
    }
    pub fn peer_key(&self) -> PublicKey {
        self.peer
    }
    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = self.tx_ctr;
        self.tx_ctr = self.tx_ctr.checked_add(1).ok_or(Error::BadPacket("compteur épuisé"))?;
        let mut out = vec![0u8; DATA_HEADER + plaintext.len() + TAG_LEN];
        out[0] = PKT_DATA;
        out[1..5].copy_from_slice(&self.sid.to_le_bytes());
        out[5..13].copy_from_slice(&nonce.to_le_bytes());
        let n = self.transport.write_message(nonce, plaintext, &mut out[DATA_HEADER..])?;
        out.truncate(DATA_HEADER + n);
        Ok(out)
    }

    pub fn open(&mut self, pkt: &[u8]) -> Result<Vec<u8>> {
        if pkt.len() < DATA_HEADER + TAG_LEN || pkt[0] != PKT_DATA {
            return Err(Error::BadPacket("paquet de données"));
        }
        if peek_sid(pkt) != Some(self.sid) {
            return Err(Error::BadPacket("session id"));
        }
        let nonce = u64::from_le_bytes(pkt[5..13].try_into().unwrap());
        if !self.replay.check(nonce) {
            return Err(Error::Replay);
        }
        let mut out = vec![0u8; pkt.len() - DATA_HEADER];
        let n = self.transport.read_message(nonce, &pkt[DATA_HEADER..], &mut out)?;
        // On ne met la fenêtre à jour qu'après authentification réussie :
        // un paquet forgé ne peut donc pas "brûler" des numéros.
        self.replay.update(nonce);
        out.truncate(n);
        Ok(out)
    }
}

/// Lit l'id de session d'un paquet DATA sans le déchiffrer (pour router vers la bonne session).
pub fn peek_sid(pkt: &[u8]) -> Option<u32> {
    if pkt.len() >= DATA_HEADER && pkt[0] == PKT_DATA {
        Some(u32::from_le_bytes(pkt[1..5].try_into().unwrap()))
    } else {
        None
    }
}

fn into_session(
    hs: HandshakeState,
    sid: u32,
    peer: PublicKey,
    peer_name: String,
) -> Result<Session> {
    let transport = hs.into_stateless_transport_mode()?;
    Ok(Session { sid, transport, tx_ctr: 0, replay: ReplayWindow::new(), peer, peer_name })
}

// ───────────────────────────── côté client ─────────────────────────────

pub struct ClientHandshake {
    hs: HandshakeState,
    host_key: PublicKey,
}

impl ClientHandshake {
    pub fn start(
        id: &Identity,
        host_key: PublicKey,
        name: &str,
        token: Option<&Token>,
        now_ms: u64,
    ) -> Result<(Self, Vec<u8>)> {
        let mut hs = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(id.secret())?
            .remote_public_key(&host_key)?
            .build_initiator()?;
        let payload = serde_json::to_vec(&InitPayload {
            v: PROTO_VERSION,
            ts_ms: now_ms,
            name: clean_name(name),
            token: token.map(|t| t.to_b32()),
        })?;
        let mut buf = vec![0u8; 1024];
        let n = hs.write_message(&payload, &mut buf)?;
        let mut pkt = Vec::with_capacity(1 + n);
        pkt.push(PKT_INIT);
        pkt.extend_from_slice(&buf[..n]);
        Ok((Self { hs, host_key }, pkt))
    }

    pub fn finish(mut self, pkt: &[u8]) -> Result<Session> {
        if pkt.first() != Some(&PKT_RESP) {
            return Err(Error::BadPacket("réponse attendue"));
        }
        let mut payload = vec![0u8; pkt.len()];
        let n = self.hs.read_message(&pkt[1..], &mut payload)?;
        let resp: RespPayload = serde_json::from_slice(&payload[..n])?;
        if resp.v != PROTO_VERSION {
            return Err(Error::BadVersion(resp.v));
        }
        if !self.hs.is_handshake_finished() {
            return Err(Error::BadPacket("poignée de main incomplète"));
        }
        into_session(self.hs, resp.sid, self.host_key, clean_name(&resp.name))
    }
}

// ───────────────────────────── côté host ─────────────────────────────

pub struct HostAuth {
    identity: Identity,
    pub trust: TrustStore,
    name: String,
    last_ts: HashMap<PublicKey, u64>,
    /// Tout INIT horodaté AVANT cette date est refusé : un INIT capturé avant le (re)démarrage du
    /// host ne peut pas être rejoué (`last_ts` n'est qu'en mémoire).
    min_ts_ms: u64,
}

impl HostAuth {
    pub fn new(identity: Identity, trust: TrustStore, name: &str) -> Self {
        Self { identity, trust, name: clean_name(name), last_ts: HashMap::new(), min_ts_ms: 0 }
    }

    /// À appeler au démarrage du host avec l'heure courante (`now_ms()`).
    pub fn set_boot_ms(&mut self, boot_ms: u64) {
        self.min_ts_ms = boot_ms;
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Traite un paquet INIT. En cas d'erreur, l'appelant ne doit RIEN renvoyer au pair
    /// (pas de réponse d'erreur : pas de scan possible, pas d'amplification).
    pub fn accept(&mut self, pkt: &[u8], now_ms: u64) -> Result<(Vec<u8>, Session)> {
        self.accept_if(pkt, now_ms, |_| true)
    }

    /// Comme [`accept`](Self::accept), mais `admit(clé du client)` est consulté APRÈS le
    /// déchiffrement et les contrôles d'horodatage, et AVANT toute conséquence (consommation du
    /// jeton, autorisation, mémorisation de l'horodatage) : un INIT refusé (`Error::Busy`) ne brûle
    /// donc rien.
    pub fn accept_if(
        &mut self,
        pkt: &[u8],
        now_ms: u64,
        admit: impl FnOnce(&PublicKey) -> bool,
    ) -> Result<(Vec<u8>, Session)> {
        if pkt.first() != Some(&PKT_INIT) {
            return Err(Error::BadPacket("INIT attendu"));
        }
        if !(MIN_INIT_LEN..=MAX_INIT_LEN).contains(&pkt.len()) {
            return Err(Error::BadPacket("taille d'INIT hors bornes"));
        }
        let mut hs = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(self.identity.secret())?
            .build_responder()?;
        let mut payload = vec![0u8; pkt.len()];
        let r = hs.read_message(&pkt[1..], &mut payload);
        let init: Result<InitPayload> = match r {
            Ok(n) => serde_json::from_slice(&payload[..n]).map_err(Into::into),
            Err(e) => Err(e.into()),
        };
        payload.zeroize(); // contient le jeton d'invitation en clair
        let init = init?;

        // Après read_message, la clé statique du client est authentifiée (es + ss).
        let client_key: PublicKey = hs
            .get_remote_static()
            .and_then(|k| k.try_into().ok())
            .ok_or(Error::BadPacket("clé client absente"))?;

        if init.v != PROTO_VERSION {
            return Err(Error::BadVersion(init.v));
        }
        if init.ts_ms.abs_diff(now_ms) > MAX_CLOCK_SKEW_MS || init.ts_ms < self.min_ts_ms {
            return Err(Error::BadTimestamp);
        }
        if self.last_ts.get(&client_key).is_some_and(|&last| init.ts_ms <= last) {
            return Err(Error::BadTimestamp);
        }
        if !admit(&client_key) {
            return Err(Error::Busy);
        }

        // Seulement maintenant (INIT authentique, frais, admis) : relecture bon marché de la
        // liste de confiance (date/longueur du fichier). Illisible = on REFUSE (fail-closed).
        self.trust.refresh()?;
        let client_name = clean_name(&init.name);
        if !self.trust.is_authorized(&client_key) {
            let token = init.token.as_deref().and_then(Token::from_b32).ok_or(Error::Unauthorized)?;
            if !self.trust.consume_invite(&token, now_ms)? {
                return Err(Error::Unauthorized);
            }
            self.trust.authorize(&client_key, &client_name, now_ms)?;
        }
        self.last_ts.insert(client_key, init.ts_ms);

        let sid = loop {
            let s = rand::rngs::OsRng.next_u32();
            if s != 0 {
                break s;
            }
        };
        let resp = serde_json::to_vec(&RespPayload { v: PROTO_VERSION, sid, name: self.name.clone() })?;
        let mut buf = vec![0u8; 512];
        let m = hs.write_message(&resp, &mut buf)?;
        let mut out = Vec::with_capacity(1 + m);
        out.push(PKT_RESP);
        out.extend_from_slice(&buf[..m]);
        Ok((out, into_session(hs, sid, client_key, client_name)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    fn host() -> HostAuth {
        HostAuth::new(Identity::generate(), TrustStore::in_memory(), "host-pc")
    }

    #[test]
    fn invite_then_reconnect_and_data() {
        let mut h = host();
        let client = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();

        let (c_hs, init) =
            ClientHandshake::start(&client, h.identity().public(), "laptop", Some(&tok), T0).unwrap();
        let (resp, mut hs_sess) = h.accept(&init, T0).unwrap();
        let mut cl_sess = c_hs.finish(&resp).unwrap();
        assert_eq!(hs_sess.sid(), cl_sess.sid());
        assert_eq!(hs_sess.peer_key(), client.public());
        assert_eq!(cl_sess.peer_key(), h.identity().public());
        assert!(h.trust.is_authorized(&client.public()));

        let p = cl_sess.seal(b"bonjour").unwrap();
        assert_eq!(hs_sess.open(&p).unwrap(), b"bonjour");
        let q = hs_sess.seal(b"salut").unwrap();
        assert_eq!(cl_sess.open(&q).unwrap(), b"salut");

        // Reconnexion sans jeton : le client est maintenant connu.
        let (c2, init2) =
            ClientHandshake::start(&client, h.identity().public(), "laptop", None, T0 + 1000).unwrap();
        let (resp2, _) = h.accept(&init2, T0 + 1000).unwrap();
        c2.finish(&resp2).unwrap();
    }

    #[test]
    fn unknown_client_is_rejected() {
        let mut h = host();
        let stranger = Identity::generate();
        let (_, init) = ClientHandshake::start(&stranger, h.identity().public(), "x", None, T0).unwrap();
        assert!(matches!(h.accept(&init, T0), Err(Error::Unauthorized)));
        assert!(!h.trust.is_authorized(&stranger.public()));
    }

    #[test]
    fn token_is_single_use_and_expires() {
        let mut h = host();
        let a = Identity::generate();
        let b = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();
        let (_, i1) = ClientHandshake::start(&a, h.identity().public(), "a", Some(&tok), T0).unwrap();
        h.accept(&i1, T0).unwrap();
        let (_, i2) = ClientHandshake::start(&b, h.identity().public(), "b", Some(&tok), T0 + 1).unwrap();
        assert!(matches!(h.accept(&i2, T0 + 1), Err(Error::Unauthorized)));

        let tok2 = h.trust.create_invite(1_000, T0).unwrap();
        let (_, i3) = ClientHandshake::start(&b, h.identity().public(), "b", Some(&tok2), T0 + 5_000).unwrap();
        assert!(matches!(h.accept(&i3, T0 + 5_000), Err(Error::Unauthorized)));
    }

    #[test]
    fn wrong_host_key_fails() {
        let mut h = host();
        let c = Identity::generate();
        let impostor = Identity::generate();
        // Le client croit parler à `impostor`, mais le paquet arrive chez le vrai host.
        let (_, init) = ClientHandshake::start(&c, impostor.public(), "c", None, T0).unwrap();
        assert!(h.accept(&init, T0).is_err());
    }

    #[test]
    fn handshake_replay_is_rejected() {
        let mut h = host();
        let c = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();
        let (_, init) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0).unwrap();
        h.accept(&init, T0).unwrap();
        assert!(matches!(h.accept(&init, T0 + 10), Err(Error::BadTimestamp)));
    }

    #[test]
    fn stale_timestamp_is_rejected() {
        let mut h = host();
        let c = Identity::generate();
        let tok = h.trust.create_invite(60_000_000, T0).unwrap();
        let (_, init) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0).unwrap();
        assert!(matches!(h.accept(&init, T0 + 10 * 60 * 1000), Err(Error::BadTimestamp)));
    }

    fn pair() -> (Session, Session) {
        let mut h = host();
        let c = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();
        let (c_hs, init) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0).unwrap();
        let (resp, hs) = h.accept(&init, T0).unwrap();
        (c_hs.finish(&resp).unwrap(), hs)
    }

    #[test]
    fn data_replay_and_tamper_are_rejected() {
        let (mut c, mut h) = pair();
        let p = c.seal(b"x").unwrap();
        assert!(h.open(&p).is_ok());
        assert!(matches!(h.open(&p), Err(Error::Replay)));

        let mut forged = c.seal(b"y").unwrap();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        assert!(h.open(&forged).is_err());
        // Le paquet altéré n'a pas consommé le numéro : l'original passe toujours.
    }

    #[test]
    fn reordering_is_tolerated() {
        let (mut c, mut h) = pair();
        let pkts: Vec<_> = (0..5u8).map(|i| c.seal(&[i]).unwrap()).collect();
        for i in [3usize, 0, 4, 1, 2] {
            assert_eq!(h.open(&pkts[i]).unwrap(), vec![i as u8]);
        }
    }

    #[test]
    fn replay_window_edges() {
        let mut w = ReplayWindow::new();
        assert!(w.check(5000));
        w.update(5000);
        assert!(!w.check(5000));
        assert!(!w.check(5000 - W)); // trop ancien
        assert!(w.check(5000 - W + 1));
        w.update(6000);
        assert!(!w.check(5000)); // sorti de la fenêtre
    }

    #[test]
    fn invite_uri_roundtrip() {
        let key = Identity::generate().public();
        let inv = crate::Invite { addr: "1.2.3.4:7777".into(), host_key: key, token: Some(Token::random()) };
        let back = crate::Invite::parse(&inv.to_uri()).unwrap();
        assert_eq!(back.addr, inv.addr);
        assert_eq!(back.host_key, key);
        assert_eq!(back.token, inv.token);
        assert!(crate::Invite::parse("http://x").is_err());
        assert!(crate::Invite::parse("grx://a:1/zzz").is_err());
    }

    #[test]
    fn init_size_bounds_match_real_packets() {
        let c = Identity::generate();
        let hk = Identity::generate().public();
        // Le plus petit INIT possible : exactement MIN_INIT_LEN.
        let (_, min) = ClientHandshake::start(&c, hk, "", None, 0).unwrap();
        assert_eq!(min.len(), MIN_INIT_LEN);
        // Le plus gros : nom au maximum de 4 octets/caractère + jeton, bien sous la borne.
        let tok = Token::random();
        let name: String = std::iter::repeat('𝄞').take(200).collect();
        let (_, max) = ClientHandshake::start(&c, hk, &name, Some(&tok), u64::MAX).unwrap();
        assert!(max.len() <= MAX_INIT_LEN, "{}", max.len());

        let mut h = host();
        let (_, ok) = ClientHandshake::start(&c, h.identity().public(), "c", None, T0).unwrap();
        assert!(ok.len() >= MIN_INIT_LEN);
        assert!(matches!(h.accept(&ok[..MIN_INIT_LEN - 1], T0), Err(Error::BadPacket(_))));
        assert!(matches!(h.accept(&[PKT_INIT; 2000], T0), Err(Error::BadPacket(_))));
        assert!(matches!(h.accept(&[PKT_INIT; 5], T0), Err(Error::BadPacket(_))));
    }

    #[test]
    fn init_older_than_host_boot_is_refused() {
        let mut h = host();
        h.set_boot_ms(T0 + 1_000);
        let c = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();
        // INIT valide mais capturé AVANT le démarrage du host : rejeu refusé.
        let (_, old) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0 + 500).unwrap();
        assert!(matches!(h.accept(&old, T0 + 2_000), Err(Error::BadTimestamp)));
        assert!(!h.trust.is_authorized(&c.public()), "le jeton ne doit pas avoir été consommé");
        // Un INIT émis après le démarrage passe (le même jeton est toujours valable).
        let (_, fresh) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0 + 1_500).unwrap();
        assert!(h.accept(&fresh, T0 + 2_000).is_ok());
    }

    #[test]
    fn refused_init_burns_nothing() {
        let mut h = host();
        let c = Identity::generate();
        let tok = h.trust.create_invite(60_000, T0).unwrap();
        let (_, i1) = ClientHandshake::start(&c, h.identity().public(), "c", Some(&tok), T0).unwrap();
        assert!(matches!(h.accept_if(&i1, T0, |_| false), Err(Error::Busy)));
        assert!(!h.trust.is_authorized(&c.public()));
        // Ni le jeton ni l'horodatage n'ont été consommés : le même INIT passe ensuite.
        assert!(h.accept_if(&i1, T0, |k| *k == c.public()).is_ok());
    }

    #[test]
    fn unreadable_trust_file_refuses_init() {
        let dir = std::env::temp_dir().join(format!("grx-sess-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("trust.json");
        let c = Identity::generate();
        let mut t = TrustStore::load(&p).unwrap();
        t.authorize(&c.public(), "c", T0).unwrap();
        let mut h = HostAuth::new(Identity::generate(), t, "h");
        let (_, i1) = ClientHandshake::start(&c, h.identity().public(), "c", None, T0).unwrap();
        assert!(h.accept(&i1, T0).is_ok());
        // Le fichier devient illisible : fail-closed même pour un client connu en mémoire.
        std::fs::write(&p, b"corrompu corrompu corrompu corrompu corrompu").unwrap();
        let (_, i2) = ClientHandshake::start(&c, h.identity().public(), "c", None, T0 + 10).unwrap();
        assert!(h.accept(&i2, T0 + 10).is_err());
        // Révocation par un autre « processus » : prise en compte à l'INIT suivant.
        std::fs::write(&p, br#"{"clients":[],"invites":[]}"#).unwrap();
        let (_, i3) = ClientHandshake::start(&c, h.identity().public(), "c", None, T0 + 20).unwrap();
        assert!(matches!(h.accept(&i3, T0 + 20), Err(Error::Unauthorized)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
