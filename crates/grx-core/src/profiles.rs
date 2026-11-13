//! Hosts mémorisés côté client (`hosts.json`), partagé entre `grx-client` et `grx-gui`.
//!
//! Format du fichier (inchangé) : `{"hosts":[{"name":..,"addr":..,"host_key":<base32>}]}`.
//! Chaque modification se fait sous un verrou de fichier exclusif avec relecture, comme `TrustStore` :
//! la GUI et `grx-client play --save-as` peuvent écrire en même temps sans s'écraser.

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
    pub addr: String,
    pub host_key: String,
}

impl Profile {
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
            Ok(b) => Ok(serde_json::from_slice(&b)?),
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

    /// Mémorise (ou remplace, à nom égal) un host. Sans nom : l'adresse sert de nom. Retourne le nom utilisé.
    pub fn remember(&mut self, invite: &Invite, save_as: Option<String>) -> Result<String> {
        let name = save_as.unwrap_or_else(|| invite.addr.clone());
        let profile = Profile { name: name.clone(), addr: invite.addr.clone(), host_key: b32_encode(&invite.host_key) };
        self.mutate(|p| {
            p.hosts.retain(|h| h.name != name);
            p.hosts.push(profile);
            Ok(((), true))
        })?;
        Ok(name)
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
