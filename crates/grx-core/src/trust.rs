use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::identity::{fingerprint, PublicKey};
use crate::invite::Token;
use crate::util::{b32_decode, b32_encode, write_atomic, FileLock};
use crate::{Error, Result};

/// Durée de validité maximale d'une invitation (24 h), quoi que demande l'appelant.
pub const MAX_INVITE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Nombre maximal d'invitations en attente (évite de gonfler `trust.json`).
const MAX_PENDING_INVITES: usize = 64;

/// Rôle d'un client autorisé.
///
/// * `ViewOnly` : voit l'écran et entend le son, n'injecte rien ;
/// * `Full` : clavier, souris, manette sur le bureau normal (comportement historique) ;
/// * `Total` : comme `Full`, et en plus, quand le host tourne en mode « Contrôle total » (host
///   lancé élevé par sa tâche planifiée), pilote clavier/souris des fenêtres administrateur au
///   premier plan. Le bureau sécurisé (invite UAC, écran de verrouillage) n'est JAMAIS visible
///   ni pilotable à distance, quel que soit le rôle : il se valide sur place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum Role {
    ViewOnly,
    #[default]
    Full,
    Total,
}

impl Role {
    /// Nom stocké dans `trust.json` et accepté par la CLI (`view_only`, `full`, `total`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::ViewOnly => "view_only",
            Role::Full => "full",
            Role::Total => "total",
        }
    }

    /// Analyse tolérante (casse, `-`/`_`) ; `None` si inconnu.
    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "view_only" | "viewonly" | "spectateur" => Some(Role::ViewOnly),
            "full" | "complet" => Some(Role::Full),
            "total" | "controle_total" => Some(Role::Total),
            _ => None,
        }
    }

    pub fn can_inject(self) -> bool {
        self != Role::ViewOnly
    }
}

impl Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    /// Un rôle inconnu (fichier écrit par une version future) devient `ViewOnly` : moindre
    /// privilège, sans rendre tout le fichier illisible.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Role, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Role::parse(&s).unwrap_or(Role::ViewOnly))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthorizedClient {
    pub key: String,
    pub name: String,
    pub added_ms: u64,
    /// Ancien champ (≤ 0.2.0) : rôle « spectateur ». Toujours écrit (`role == ViewOnly`) pour
    /// qu'une version ancienne lise un rôle sûr : un client `total` y est vu comme `full`.
    #[serde(default)]
    pub view_only: bool,
    /// Rôle (≥ 0.3.0). Absent = déduit de `view_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
}

impl AuthorizedClient {
    /// Rôle effectif. `view_only = true` l'emporte toujours (le plus restrictif gagne si les deux
    /// champs se contredisent, par exemple après une modification par une version ancienne).
    pub fn role(&self) -> Role {
        if self.view_only {
            Role::ViewOnly
        } else {
            self.role.unwrap_or(Role::Full)
        }
    }

    fn set_role(&mut self, r: Role) {
        self.view_only = r == Role::ViewOnly;
        self.role = Some(r);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingInvite {
    hash: String,
    expires_ms: u64,
}

/// Demande d'accès reçue sans invitation (voir `pairing`), en attente d'une décision sur le host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AccessRequest {
    /// Clé publique du client (base32).
    pub key: String,
    /// Nom annoncé par le client (non vérifié).
    pub name: String,
    /// Dernière adresse IP:port d'où la demande est arrivée.
    pub addr: String,
    pub first_ms: u64,
    /// Dernière relance du client (il redemande toutes les ~2 s ; écrit au plus toutes les 15 s).
    pub last_ms: u64,
    /// Refusée : le client reçoit « refusé » jusqu'à expiration (`DENIED_TTL_MS`).
    #[serde(default)]
    pub denied: bool,
}

impl AccessRequest {
    pub fn fingerprint(&self) -> String {
        b32_decode(&self.key).and_then(|v| <[u8; 32]>::try_from(v).ok()).map(|k| fingerprint(&k)).unwrap_or_default()
    }

    fn expired(&self, now_ms: u64) -> bool {
        let ttl = if self.denied { DENIED_TTL_MS } else { REQUEST_TTL_MS };
        now_ms.saturating_sub(self.last_ms) > ttl
    }
}

/// Demandes conservées au plus (les plus anciennes sont oubliées).
pub const MAX_ACCESS_REQUESTS: usize = 16;
/// Une demande que le client ne relance plus est oubliée après 10 min.
pub const REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
/// Un refus est retenu 1 h (le client reçoit « refusé » au lieu de redemander).
pub const DENIED_TTL_MS: u64 = 60 * 60 * 1000;
/// `last_ms` n'est réécrit sur disque qu'au-delà de cet écart (le client relance toutes les 2 s).
const REQUEST_TOUCH_MS: u64 = 15 * 1000;
/// Valeur de `requests_open_until_ms` pour « ouvert sans limite de durée ».
pub const REQUESTS_ALWAYS_OPEN: u64 = u64::MAX;

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Serialize, Deserialize, Default)]
struct Data {
    clients: Vec<AuthorizedClient>,
    invites: Vec<PendingInvite>,
    /// Demandes d'accès sans invitation (ignorées, et perdues à la réécriture, par une version ancienne).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requests: Vec<AccessRequest>,
    /// Les demandes d'accès sont acceptées (le host répond aux inconnus) jusqu'à cette date ;
    /// 0 = fermées (défaut : le host reste muet envers les inconnus).
    #[serde(default, skip_serializing_if = "is_zero")]
    requests_open_until_ms: u64,
}

/// Empreinte bon marché du fichier (date de modification + longueur) pour détecter un changement.
type Stamp = Option<(SystemTime, u64)>;

fn stamp_of(path: &Path) -> std::io::Result<Stamp> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some((m.modified()?, m.len()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Liste de confiance du host : qui a le droit de se connecter.
/// C'est l'équivalent de `authorized_keys` en SSH, à la place des comptes Parsec.
///
/// Toute modification (`authorize`, `revoke`, `create_invite`, `consume_invite`, `set_view_only`)
/// se fait sous un verrou de fichier exclusif et RELIT le fichier dans le verrou : deux
/// processus (CLI `invite`/`revoke` et `run`) ne s'écrasent donc jamais mutuellement.
pub struct TrustStore {
    path: Option<PathBuf>,
    data: Data,
    stamp: Stamp,
}

impl TrustStore {
    pub fn in_memory() -> Self {
        Self { path: None, data: Data::default(), stamp: None }
    }

    /// Lit le fichier (verrou partagé : jamais un fichier à moitié écrit). Une erreur de lecture
    /// ou un JSON invalide est une ERREUR (fail-closed), pas une liste vide.
    pub fn load(path: &Path) -> Result<Self> {
        let (data, stamp) = Self::read_locked(path)?;
        Ok(Self { path: Some(path.to_path_buf()), data, stamp })
    }

    fn read_locked(path: &Path) -> Result<(Data, Stamp)> {
        let _lock = FileLock::acquire(path, false)?;
        Self::read_file(path)
    }

    fn read_file(path: &Path) -> Result<(Data, Stamp)> {
        let stamp = stamp_of(path)?;
        let data = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e.into()),
        };
        Ok((data, stamp))
    }

    /// Relit le fichier seulement s'il a changé (date/longueur). Appel bon marché : une seule
    /// requête `metadata` quand rien n'a bougé. Retourne `true` si la liste a été rechargée.
    /// En cas d'erreur, la liste en mémoire n'est PAS considérée comme fiable : l'appelant doit
    /// refuser (fail-closed).
    pub fn refresh(&mut self) -> Result<bool> {
        let Some(path) = self.path.clone() else { return Ok(false) };
        let st = stamp_of(&path)?;
        // Granularité d'horloge du système de fichiers : un fichier modifié très récemment peut
        // l'avoir été sans changer ni date ni longueur observables, on relit alors par prudence.
        let recent = st.is_some_and(|(m, _)| {
            SystemTime::now().duration_since(m).map(|d| d < Duration::from_secs(2)).unwrap_or(true)
        });
        if st == self.stamp && !recent {
            return Ok(false);
        }
        let (data, stamp) = Self::read_locked(&path)?;
        self.data = data;
        self.stamp = stamp;
        Ok(true)
    }

    /// Cycle verrou exclusif -> relecture -> modification -> sauvegarde. `f` retourne le résultat
    /// et un booléen « modifié ».
    fn mutate<R>(&mut self, f: impl FnOnce(&mut Data) -> Result<(R, bool)>) -> Result<R> {
        let Some(path) = self.path.clone() else {
            return Ok(f(&mut self.data)?.0);
        };
        let _lock = FileLock::acquire(&path, true)?;
        let (mut data, _) = Self::read_file(&path)?;
        let (r, changed) = f(&mut data)?;
        if changed {
            write_atomic(&path, &serde_json::to_vec_pretty(&data)?)?;
        }
        self.stamp = stamp_of(&path)?;
        self.data = data;
        Ok(r)
    }

    pub fn is_authorized(&self, key: &PublicKey) -> bool {
        let k = b32_encode(key);
        self.data.clients.iter().any(|c| c.key == k)
    }

    /// Rôle « spectateur » du client (faux s'il est inconnu).
    pub fn is_view_only(&self, key: &PublicKey) -> bool {
        self.role_of(key) == Some(Role::ViewOnly)
    }

    /// Rôle du client, `None` s'il n'est pas autorisé.
    pub fn role_of(&self, key: &PublicKey) -> Option<Role> {
        let k = b32_encode(key);
        self.data.clients.iter().find(|c| c.key == k).map(AuthorizedClient::role)
    }

    pub fn authorize(&mut self, key: &PublicKey, name: &str, now_ms: u64) -> Result<()> {
        let k = b32_encode(key);
        self.mutate(|d| {
            if d.clients.iter().any(|c| c.key == k) {
                return Ok(((), false));
            }
            d.clients.push(AuthorizedClient { key: k, name: name.to_string(), added_ms: now_ms, view_only: false, role: None });
            Ok(((), true))
        })
    }

    fn matching(fingerprint_or_prefix: &str) -> Result<String> {
        let want = fingerprint_or_prefix.trim().to_uppercase();
        // Un préfixe vide ou trop court correspondrait à (presque) tout le monde.
        if want.chars().filter(|c| *c != '-').count() < 4 {
            return Err(Error::BadInvite("préfixe d'empreinte trop court (4 caractères minimum)".into()));
        }
        Ok(want)
    }

    fn fp_of(c: &AuthorizedClient) -> String {
        b32_decode(&c.key).and_then(|v| <[u8; 32]>::try_from(v).ok()).map(|k| fingerprint(&k)).unwrap_or_default()
    }

    pub fn revoke(&mut self, fingerprint_or_prefix: &str) -> Result<usize> {
        let want = Self::matching(fingerprint_or_prefix)?;
        self.mutate(|d| {
            let before = d.clients.len();
            d.clients.retain(|c| !Self::fp_of(c).starts_with(&want));
            let n = before - d.clients.len();
            Ok((n, n > 0))
        })
    }

    /// Bascule spectateur / complet (ancienne API, voir [`Self::set_role`]).
    pub fn set_view_only(&mut self, fingerprint_or_prefix: &str, view_only: bool) -> Result<usize> {
        self.set_role(fingerprint_or_prefix, if view_only { Role::ViewOnly } else { Role::Full })
    }

    /// Change le rôle des clients dont l'empreinte commence par le préfixe. Retourne leur nombre.
    pub fn set_role(&mut self, fingerprint_or_prefix: &str, role: Role) -> Result<usize> {
        let want = Self::matching(fingerprint_or_prefix)?;
        self.mutate(|d| {
            let mut n = 0;
            let mut changed = false;
            for c in d.clients.iter_mut().filter(|c| Self::fp_of(c).starts_with(&want)) {
                n += 1;
                if c.role() != role || c.role != Some(role) {
                    c.set_role(role);
                    changed = true;
                }
            }
            Ok((n, changed))
        })
    }

    pub fn clients(&self) -> &[AuthorizedClient] {
        &self.data.clients
    }

    // ───────────── demandes d'accès sans invitation ─────────────

    /// Les demandes d'accès sont-elles ouvertes à cette date ?
    pub fn requests_open(&self, now_ms: u64) -> bool {
        now_ms < self.data.requests_open_until_ms
    }

    /// Date de fermeture des demandes (0 = fermées, [`REQUESTS_ALWAYS_OPEN`] = sans limite).
    pub fn requests_open_until(&self) -> u64 {
        self.data.requests_open_until_ms
    }

    /// Ouvre (date future, ou [`REQUESTS_ALWAYS_OPEN`]) ou ferme (0) les demandes d'accès.
    pub fn set_requests_open_until(&mut self, until_ms: u64) -> Result<()> {
        self.mutate(|d| {
            let changed = d.requests_open_until_ms != until_ms;
            d.requests_open_until_ms = until_ms;
            Ok(((), changed))
        })
    }

    /// Demandes non expirées (en attente et refusées), les plus récentes d'abord.
    pub fn access_requests(&self, now_ms: u64) -> Vec<AccessRequest> {
        let mut v: Vec<_> = self.data.requests.iter().filter(|r| !r.expired(now_ms)).cloned().collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.first_ms));
        v
    }

    /// Une demande (en attente ou refusée, non expirée) existe-t-elle pour cette clé ?
    pub fn has_request(&self, key: &PublicKey) -> bool {
        let k = b32_encode(key);
        self.data.requests.iter().any(|r| r.key == k)
    }

    /// Le host a-t-il refusé (récemment) cette clé ?
    pub fn is_request_denied(&self, key: &PublicKey, now_ms: u64) -> bool {
        let k = b32_encode(key);
        self.data.requests.iter().any(|r| r.key == k && r.denied && !r.expired(now_ms))
    }

    /// Enregistre (ou rafraîchit) la demande d'accès d'une clé inconnue. N'écrit sur disque que
    /// si la demande est nouvelle, a changé d'adresse ou de nom, ou n'a pas été touchée depuis 15 s.
    pub fn note_request(&mut self, key: &PublicKey, name: &str, addr: &str, now_ms: u64) -> Result<()> {
        let k = b32_encode(key);
        // Chemin rapide sans verrou exclusif ni écriture (relances toutes les 2 s).
        if let Some(r) = self.data.requests.iter().find(|r| r.key == k && !r.denied) {
            if r.name == name && r.addr == addr && now_ms.saturating_sub(r.last_ms) < REQUEST_TOUCH_MS {
                return Ok(());
            }
        }
        self.mutate(|d| {
            d.requests.retain(|r| !r.expired(now_ms));
            if let Some(r) = d.requests.iter_mut().find(|r| r.key == k) {
                if r.denied {
                    return Ok(((), false));
                }
                r.name = name.to_string();
                r.addr = addr.to_string();
                r.last_ms = now_ms;
                return Ok(((), true));
            }
            if d.requests.len() >= MAX_ACCESS_REQUESTS {
                // On oublie la demande en attente la plus ancienne (sinon le refus le plus ancien).
                let victim = d
                    .requests
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.denied)
                    .min_by_key(|(_, r)| r.last_ms)
                    .or_else(|| d.requests.iter().enumerate().min_by_key(|(_, r)| r.last_ms))
                    .map(|(i, _)| i);
                if let Some(i) = victim {
                    d.requests.remove(i);
                }
            }
            d.requests.push(AccessRequest {
                key: k.clone(),
                name: name.to_string(),
                addr: addr.to_string(),
                first_ms: now_ms,
                last_ms: now_ms,
                denied: false,
            });
            Ok(((), true))
        })
    }

    /// Accepte la demande dont l'empreinte est EXACTEMENT `fp` : la clé devient un client autorisé
    /// avec ce rôle. Retourne le nom du client, `None` si la demande n'existe plus.
    pub fn approve_request(&mut self, fp: &str, role: Role, now_ms: u64) -> Result<Option<String>> {
        let want = fp.trim().to_uppercase();
        self.mutate(|d| {
            let Some(i) = d.requests.iter().position(|r| r.fingerprint() == want && !r.expired(now_ms)) else {
                return Ok((None, false));
            };
            let r = d.requests.remove(i);
            if !d.clients.iter().any(|c| c.key == r.key) {
                let mut c = AuthorizedClient { key: r.key, name: r.name.clone(), added_ms: now_ms, view_only: false, role: None };
                c.set_role(role);
                d.clients.push(c);
            }
            Ok((Some(r.name), true))
        })
    }

    /// Refuse la demande dont l'empreinte est EXACTEMENT `fp` (retenue 1 h). `false` si elle
    /// n'existe plus.
    pub fn deny_request(&mut self, fp: &str, now_ms: u64) -> Result<bool> {
        let want = fp.trim().to_uppercase();
        self.mutate(|d| {
            let Some(r) = d.requests.iter_mut().find(|r| r.fingerprint() == want && !r.expired(now_ms)) else {
                return Ok((false, false));
            };
            r.denied = true;
            r.last_ms = now_ms;
            Ok((true, true))
        })
    }

    /// Oublie une demande (en attente ou refusée) : le client pourra redemander.
    pub fn forget_request(&mut self, fp: &str) -> Result<bool> {
        let want = fp.trim().to_uppercase();
        self.mutate(|d| {
            let before = d.requests.len();
            d.requests.retain(|r| r.fingerprint() != want);
            let n = d.requests.len() < before;
            Ok((n, n))
        })
    }

    pub fn create_invite(&mut self, ttl_ms: u64, now_ms: u64) -> Result<Token> {
        let ttl = ttl_ms.min(MAX_INVITE_TTL_MS);
        let t = Token::random();
        let hash = b32_encode(&t.hash());
        self.mutate(|d| {
            d.invites.retain(|i| i.expires_ms > now_ms);
            if d.invites.len() >= MAX_PENDING_INVITES {
                d.invites.remove(0);
            }
            d.invites.push(PendingInvite { hash, expires_ms: now_ms.saturating_add(ttl) });
            Ok(((), true))
        })?;
        Ok(t)
    }

    /// Consomme un jeton : valable une seule fois, jamais après expiration.
    pub fn consume_invite(&mut self, token: &Token, now_ms: u64) -> Result<bool> {
        let h = b32_encode(&token.hash());
        self.mutate(|d| {
            d.invites.retain(|i| i.expires_ms > now_ms);
            let before = d.invites.len();
            d.invites.retain(|i| i.hash != h);
            let used = d.invites.len() < before;
            Ok((used, used))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::sync::{Arc, Barrier};

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("grx-trust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ttl_is_capped_and_saturating() {
        let mut t = TrustStore::in_memory();
        let now = 1_700_000_000_000;
        let tok = t.create_invite(u64::MAX, now).unwrap();
        // Au-delà du plafond de 24 h : expiré.
        assert!(!t.consume_invite(&tok, now + MAX_INVITE_TTL_MS + 1).unwrap());
        let tok = t.create_invite(u64::MAX, now).unwrap();
        assert!(t.consume_invite(&tok, now + MAX_INVITE_TTL_MS - 1).unwrap());
        // Pas de débordement même avec `now` énorme.
        t.create_invite(u64::MAX, u64::MAX - 1).unwrap();
    }

    #[test]
    fn view_only_role_roundtrip_and_default() {
        let dir = tmp_dir("role");
        let p = dir.join("trust.json");
        let k = Identity::generate().public();
        let mut t = TrustStore::load(&p).unwrap();
        t.authorize(&k, "pc", 1).unwrap();
        assert!(!t.is_view_only(&k));
        let fp = fingerprint(&k);
        assert_eq!(t.set_view_only(&fp[..9], true).unwrap(), 1);
        let t2 = TrustStore::load(&p).unwrap();
        assert!(t2.is_view_only(&k));
        // Ancien format sans le champ `view_only` : valeur par défaut.
        std::fs::write(
            &p,
            format!(r#"{{"clients":[{{"key":"{}","name":"x","added_ms":1}}],"invites":[]}}"#, b32_encode(&k)),
        )
        .unwrap();
        let t3 = TrustStore::load(&p).unwrap();
        assert!(t3.is_authorized(&k) && !t3.is_view_only(&k));
        assert!(TrustStore::in_memory().set_view_only("", true).is_err()); // préfixe vide refusé
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_or_corrupt_file_is_an_error_not_an_empty_list() {
        let dir = tmp_dir("corrupt");
        let p = dir.join("trust.json");
        std::fs::write(&p, b"{ pas du json").unwrap();
        assert!(TrustStore::load(&p).is_err());
        // Un fichier valide devenu illisible après coup : `refresh` échoue (fail-closed) et la
        // mutation aussi (on n'écrase pas un fichier qu'on ne comprend pas).
        std::fs::write(&p, br#"{"clients":[],"invites":[]}"#).unwrap();
        let mut t = TrustStore::load(&p).unwrap();
        std::fs::write(&p, b"corrompu et plus long que l'original").unwrap();
        assert!(t.refresh().is_err());
        assert!(t.create_invite(1000, 1).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_only_rereads_on_change() {
        let dir = tmp_dir("refresh");
        let p = dir.join("trust.json");
        let k = Identity::generate().public();
        let mut writer = TrustStore::load(&p).unwrap();
        let mut reader = TrustStore::load(&p).unwrap();
        writer.authorize(&k, "pc", 1).unwrap();
        assert!(!reader.is_authorized(&k));
        assert!(reader.refresh().unwrap());
        assert!(reader.is_authorized(&k));
        writer.revoke(&fingerprint(&k)).unwrap();
        reader.refresh().unwrap();
        assert!(!reader.is_authorized(&k));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deux threads (deux `TrustStore` indépendants, comme deux processus) font des cycles
    /// lecture-modification-écriture entrelacés : aucun jeton consommé ni client révoqué ne
    /// doit être « ressuscité » par une écriture à partir d'un état périmé.
    #[test]
    fn concurrent_invite_consume_revoke_never_resurrects() {
        let dir = tmp_dir("conc");
        let p = dir.join("trust.json");
        let barrier = Arc::new(Barrier::new(2));
        const N: usize = 40;

        let (p1, b1) = (p.clone(), barrier.clone());
        let a = std::thread::spawn(move || {
            let mut t = TrustStore::load(&p1).unwrap();
            b1.wait();
            let mut toks = Vec::new();
            for _ in 0..N {
                let tok = t.create_invite(60_000, 1_000).unwrap();
                assert!(t.consume_invite(&tok, 1_001).unwrap(), "jeton perdu");
                assert!(!t.consume_invite(&tok, 1_002).unwrap(), "jeton consommé deux fois");
                toks.push(tok);
            }
            toks
        });
        let (p2, b2) = (p.clone(), barrier.clone());
        let b = std::thread::spawn(move || {
            let mut t = TrustStore::load(&p2).unwrap();
            b2.wait();
            let mut keys = Vec::new();
            for i in 0..N {
                let k = Identity::generate().public();
                t.authorize(&k, &format!("c{i}"), 1).unwrap();
                assert_eq!(t.revoke(&fingerprint(&k)).unwrap(), 1, "client perdu");
                keys.push(k);
            }
            keys
        });
        let toks = a.join().unwrap();
        let keys = b.join().unwrap();

        let mut fin = TrustStore::load(&p).unwrap();
        assert!(fin.clients().is_empty(), "client révoqué ressuscité");
        for k in &keys {
            assert!(!fin.is_authorized(k));
        }
        for tok in &toks {
            assert!(!fin.consume_invite(tok, 1_003).unwrap(), "jeton consommé ressuscité");
        }
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(!names.iter().any(|n| n.to_string_lossy().ends_with(".tmp")), "temporaire résiduel: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[path = "trust_role_tests.rs"]
mod role_tests;

#[cfg(test)]
#[path = "trust_request_tests.rs"]
mod request_tests;
