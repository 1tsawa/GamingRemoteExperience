//! Hosts mémorisés côté client (`hosts.json`), partagé entre `grx-client` et `grx-gui`.
//!
//! Format du fichier : `{"hosts":[{"name":..,"addr":..,"addrs":[..],"host_key":<base32>}]}`.
//! Un host = une identité (clé publique épinglée) et une LISTE ORDONNÉE d'adresses (réseau local,
//! Internet, ancien et nouveau port…) essayées dans l'ordre : changer d'adresse ne demande jamais
//! de réappairer, et aucune adresse ne peut usurper le host (la clé est vérifiée par Noise).
//! Rétrocompatibilité : `addr` reste écrit (= première adresse, seule lue par les versions < 0.2.4)
//! et `addrs` n'est écrit que s'il y a plusieurs adresses ; un ancien fichier (sans `addrs`) se lit.
//! Chaque modification se fait sous un verrou de fichier exclusif avec relecture, comme `TrustStore` :
//! la GUI et `grx-client play --save-as` peuvent écrire en même temps sans s'écraser.

use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::identity::{fingerprint, PublicKey};
use crate::invite::Invite;
use crate::util::{b32_decode, b32_encode, write_atomic, FileLock};
use crate::{Error, Result};

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Profiles {
    pub hosts: Vec<Profile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// Adresse principale (= `addrs[0]`), la seule connue des versions < 0.2.4.
    pub addr: String,
    /// Toutes les adresses, dans l'ordre d'essai (vide = seulement `addr`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub host_key: String,
}

/// Adresses conservées au plus par host.
pub const MAX_ADDRS: usize = 8;

/// Vérifie une adresse de host `hôte:port` (IPv4, `[IPv6]:port` ou nom DNS, port 1-65535) et la
/// renvoie nettoyée ; sinon un message clair pour l'utilisateur.
pub fn validate_addr(input: &str) -> std::result::Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("Indiquez l'adresse du host (ex. 203.0.113.5:443 ou monpc.duckdns.org:443).".into());
    }
    if s.contains("://") || s.contains('/') || s.chars().any(char::is_whitespace) {
        return Err("L'adresse doit être de la forme hôte:port, sans « grx:// », sans « / » ni espace.".into());
    }
    let (host, port) =
        s.rsplit_once(':').ok_or_else(|| "Le port est manquant : écrivez hôte:port (ex. 203.0.113.5:443).".to_string())?;
    let port: u16 = port
        .parse()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| "Port invalide (nombre entre 1 et 65535 attendu).".to_string())?;
    if host.is_empty() {
        return Err("L'hôte est manquant avant le « : ».".into());
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let ip: Ipv6Addr = inner.parse().map_err(|_| "Adresse IPv6 invalide entre crochets.".to_string())?;
        if ip.is_unspecified() {
            return Err("« :: » n'est pas une adresse joignable par un client.".into());
        }
    } else {
        if host.contains(':') {
            return Err("Une adresse IPv6 doit être écrite entre crochets : [2a01:db8::1]:443.".into());
        }
        if host.parse::<IpAddr>().map(|ip| ip.is_unspecified()).unwrap_or(false) {
            return Err("0.0.0.0 est l'adresse d'écoute du host, pas une adresse joignable par un client : utilisez l'IP ou le nom DNS du PC.".into());
        }
        let ok = host.len() <= 253
            && host.split('.').all(|l| {
                !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            });
        if !ok {
            return Err("Hôte invalide : lettres, chiffres, « - » et « . » uniquement.".into());
        }
    }
    Ok(format!("{host}:{port}"))
}

/// Valide, nettoie et dédoublonne une liste d'adresses (ordre conservé). Au moins une, au plus
/// [`MAX_ADDRS`].
pub fn validate_addrs<S: AsRef<str>>(list: &[S]) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for a in list {
        let v = validate_addr(a.as_ref()).map_err(|e| format!("« {} » : {e}", a.as_ref().trim()))?;
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&v)) {
            out.push(v);
        }
    }
    if out.is_empty() {
        return Err("Au moins une adresse est nécessaire.".into());
    }
    if out.len() > MAX_ADDRS {
        return Err(format!("{MAX_ADDRS} adresses au plus."));
    }
    Ok(out)
}

impl Profile {
    /// Adresses dans l'ordre d'essai (au moins `addr`).
    pub fn addresses(&self) -> Vec<String> {
        if self.addrs.is_empty() {
            vec![self.addr.clone()]
        } else {
            self.addrs.clone()
        }
    }

    /// Fixe la liste (déjà validée, non vide) : `addr` = première, `addrs` écrit seulement si > 1.
    fn set_addresses(&mut self, list: Vec<String>) {
        self.addr = list.first().cloned().unwrap_or_default();
        self.addrs = if list.len() > 1 { list } else { Vec::new() };
    }

    pub fn public_key(&self) -> Option<PublicKey> {
        b32_decode(&self.host_key).and_then(|v| v.try_into().ok())
    }

    /// Empreinte lisible du host, `None` si la clé mémorisée est corrompue.
    pub fn fingerprint(&self) -> Option<String> {
        self.public_key().map(|k| fingerprint(&k))
    }
}

pub struct ProfileStore {
    path: PathBuf,
    profiles: Profiles,
}

impl ProfileStore {
    pub const FILE: &'static str = "hosts.json";

    /// Charge `<dir>/hosts.json`. Fichier absent = liste vide ; fichier illisible ou JSON invalide = erreur
    /// (on n'écrase jamais silencieusement un fichier qu'on ne sait pas lire).
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let profiles = Self::read(&path)?;
        Ok(Self { path, profiles })
    }

    fn read(path: &Path) -> Result<Profiles> {
        match std::fs::read(path) {
            Ok(b) => {
                let mut p: Profiles = serde_json::from_slice(&b)?;
                // `addr` modifié par une ancienne version (qui ignore `addrs`) : il passe en tête.
                for h in &mut p.hosts {
                    if !h.addrs.is_empty() && h.addrs.first() != Some(&h.addr) {
                        let mut l = vec![h.addr.clone()];
                        l.extend(h.addrs.iter().filter(|a| **a != h.addr).cloned());
                        l.truncate(MAX_ADDRS);
                        h.set_addresses(l);
                    }
                }
                Ok(p)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Profiles::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn hosts(&self) -> &[Profile] {
        &self.profiles.hosts
    }

    /// Relit le fichier (verrou partagé).
    pub fn reload(&mut self) -> Result<()> {
        let _lock = FileLock::acquire(&self.path, false)?;
        self.profiles = Self::read(&self.path)?;
        Ok(())
    }

    fn mutate<R>(&mut self, f: impl FnOnce(&mut Profiles) -> Result<(R, bool)>) -> Result<R> {
        let _lock = FileLock::acquire(&self.path, true)?;
        let mut p = Self::read(&self.path)?;
        let (r, changed) = f(&mut p)?;
        if changed {
            write_atomic(&self.path, &serde_json::to_vec_pretty(&p)?)?;
        }
        self.profiles = p;
        Ok(r)
    }

    /// Lien d'invitation ou nom mémorisé -> `Invite` ; `bool` = host déjà connu (donc sans jeton).
    pub fn resolve(&self, target: &str) -> Result<(Invite, bool)> {
        if target.starts_with("grx://") {
            return Ok((Invite::parse(target)?, false));
        }
        let p = self
            .profiles
            .hosts
            .iter()
            .find(|h| h.name == target)
            .ok_or_else(|| Error::BadInvite(format!("host inconnu: {target}")))?;
        let host_key = p.public_key().ok_or_else(|| Error::BadInvite("profil corrompu".into()))?;
        Ok((Invite { addr: p.addr.clone(), host_key, token: None }, true))
    }

    /// Adresses (dans l'ordre d'essai) du host mémorisé `name`.
    pub fn addresses(&self, name: &str) -> Option<Vec<String>> {
        self.profiles.hosts.iter().find(|h| h.name == name).map(Profile::addresses)
    }

    /// Mémorise (ou remplace, à nom égal) un host. Sans nom : l'adresse sert de nom. Retourne le nom
    /// utilisé. Même nom ET même clé : l'adresse de l'invitation passe en tête, les autres restent.
    pub fn remember(&mut self, invite: &Invite, save_as: Option<String>) -> Result<String> {
        let name = save_as.unwrap_or_else(|| invite.addr.clone());
        let key = b32_encode(&invite.host_key);
        self.mutate(|p| {
            let mut list = vec![invite.addr.clone()];
            if let Some(old) = p.hosts.iter().find(|h| h.name == name && h.host_key == key) {
                list.extend(old.addresses().into_iter().filter(|a| *a != invite.addr));
                list.truncate(MAX_ADDRS);
            }
            p.hosts.retain(|h| h.name != name);
            let mut profile = Profile { name: name.clone(), addr: String::new(), addrs: Vec::new(), host_key: key };
            profile.set_addresses(list);
            p.hosts.push(profile);
            Ok(((), true))
        })?;
        Ok(name)
    }

    /// Remplace la liste d'adresses de `name` (validée : voir [`validate_addrs`]). L'identité du
    /// host (sa clé) ne change pas : pas besoin de réappairer. `false` si `name` n'existe plus.
    pub fn set_addresses<S: AsRef<str>>(&mut self, name: &str, addrs: &[S]) -> Result<bool> {
        let list = validate_addrs(addrs).map_err(Error::BadInvite)?;
        self.mutate(|p| match p.hosts.iter_mut().find(|h| h.name == name) {
            Some(h) => {
                let changed = h.addresses() != list;
                h.set_addresses(list);
                Ok((true, changed))
            }
            None => Ok((false, false)),
        })
    }

    /// Ajoute une adresse (en tête si `first`, sinon à la fin ; déplacée si déjà présente).
    pub fn add_address(&mut self, name: &str, addr: &str, first: bool) -> Result<bool> {
        let a = validate_addr(addr).map_err(Error::BadInvite)?;
        let Some(mut list) = self.addresses(name) else { return Ok(false) };
        list.retain(|x| !x.eq_ignore_ascii_case(&a));
        if first {
            list.insert(0, a);
        } else {
            list.push(a);
        }
        self.set_addresses(name, &list)
    }

    /// Retire une adresse. Refuse de retirer la dernière.
    pub fn remove_address(&mut self, name: &str, addr: &str) -> Result<bool> {
        let Some(mut list) = self.addresses(name) else { return Ok(false) };
        let before = list.len();
        list.retain(|x| !x.eq_ignore_ascii_case(addr.trim()));
        if list.len() == before {
            return Err(Error::BadInvite(format!("« {} » n'est pas une adresse de « {name} »", addr.trim())));
        }
        if list.is_empty() {
            return Err(Error::BadInvite("impossible de retirer la dernière adresse (supprimez plutôt le host)".into()));
        }
        self.set_addresses(name, &list)
    }

    /// Modification complète (fenêtre « Modifier » de la GUI) : nom et adresses, en une écriture.
    /// Refuse un nom déjà pris ou vide. `false` si `old` n'existe plus.
    pub fn edit<S: AsRef<str>>(&mut self, old: &str, new_name: &str, addrs: &[S]) -> Result<bool> {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            return Err(Error::BadInvite("le nom ne peut pas être vide".into()));
        }
        let list = validate_addrs(addrs).map_err(Error::BadInvite)?;
        self.mutate(|p| {
            if old != new_name && p.hosts.iter().any(|h| h.name == new_name) {
                return Err(Error::BadInvite(format!("le nom « {new_name} » est déjà utilisé")));
            }
            match p.hosts.iter_mut().find(|h| h.name == old) {
                Some(h) => {
                    let changed = h.name != new_name || h.addresses() != list;
                    h.name = new_name.clone();
                    h.set_addresses(list);
                    Ok((true, changed))
                }
                None => Ok((false, false)),
            }
        })
    }

    /// Renomme un host. Refuse un nom déjà pris. `false` si `old` n'existe plus.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<bool> {
        self.mutate(|p| {
            if old != new && p.hosts.iter().any(|h| h.name == new) {
                return Err(Error::BadInvite(format!("le nom « {new} » est déjà utilisé")));
            }
            match p.hosts.iter_mut().find(|h| h.name == old) {
                Some(h) => {
                    let changed = h.name != new;
                    h.name = new.to_string();
                    Ok((true, changed))
                }
                None => Ok((false, false)),
            }
        })
    }

    /// Supprime un host mémorisé. `false` s'il n'existait plus.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        self.mutate(|p| {
            let before = p.hosts.len();
            p.hosts.retain(|h| h.name != name);
            let removed = p.hosts.len() < before;
            Ok((removed, removed))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("grx-prof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn remember_rename_remove_roundtrip_and_old_format() {
        let dir = tmp_dir("rt");
        let k = Identity::generate().public();
        let inv = Invite { addr: "1.2.3.4:7777".into(), host_key: k, token: None };
        let mut s = ProfileStore::load(&dir).unwrap();
        assert!(s.hosts().is_empty());
        assert_eq!(s.remember(&inv, Some("pc".into())).unwrap(), "pc");
        assert_eq!(s.remember(&inv, None).unwrap(), "1.2.3.4:7777");
        assert!(s.rename("pc", "salon").unwrap());
        assert!(s.rename("salon", "1.2.3.4:7777").is_err(), "nom déjà pris");
        assert!(!s.rename("absent", "x").unwrap());
        let s2 = ProfileStore::load(&dir).unwrap();
        assert_eq!(s2.hosts().len(), 2);
        let (i, known) = s2.resolve("salon").unwrap();
        assert!(known && i.host_key == k && i.addr == "1.2.3.4:7777");
        assert_eq!(s2.hosts()[1].fingerprint().unwrap(), fingerprint(&k));
        let mut s2 = s2;
        assert!(s2.remove("salon").unwrap() && !s2.remove("salon").unwrap());
        assert!(ProfileStore::load(&dir).unwrap().resolve("salon").is_err());
        // format identique à l'ancien hosts.json
        std::fs::write(
            dir.join("hosts.json"),
            br#"{"hosts":[{"name":"a","addr":"h:1","host_key":"aaaa"}]}"#,
        )
        .unwrap();
        assert_eq!(ProfileStore::load(&dir).unwrap().hosts()[0].name, "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_an_error_not_overwritten() {
        let dir = tmp_dir("bad");
        std::fs::write(dir.join("hosts.json"), b"{pas du json").unwrap();
        assert!(ProfileStore::load(&dir).is_err());
        assert_eq!(std::fs::read(dir.join("hosts.json")).unwrap(), b"{pas du json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[path = "profiles_addrs_tests.rs"]
mod addrs_tests;
