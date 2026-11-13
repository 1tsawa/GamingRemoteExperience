use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::util::{b32_decode, b32_encode, write_atomic};
use crate::{Error, Result};

pub const NOISE_PARAMS: &str = "Noise_IK_25519_AESGCM_SHA256";

pub type PublicKey = [u8; 32];

/// Identité de la machine (host ou client). La clé publique EST l'identité :
/// il n'y a ni compte, ni serveur d'authentification.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Identity {
    secret: [u8; 32],
    #[zeroize(skip)]
    public: PublicKey,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct IdentityFile {
    secret: String,
    public: String,
}

impl Identity {
    pub fn generate() -> Self {
        let params = NOISE_PARAMS.parse().expect("params noise valides");
        let mut kp = snow::Builder::new(params)
            .generate_keypair()
            .expect("génération de clé");
        let id = Self {
            secret: kp.private.as_slice().try_into().expect("32 octets"),
            public: kp.public.as_slice().try_into().expect("32 octets"),
        };
        kp.private.zeroize();
        id
    }

    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(mut bytes) => {
                let r = Self::from_file_bytes(&bytes);
                bytes.zeroize(); // le fichier contient la clé privée
                r
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate();
                let f = IdentityFile {
                    secret: b32_encode(&id.secret),
                    public: b32_encode(&id.public),
                };
                let mut json = serde_json::to_vec_pretty(&f)?;
                let res = write_atomic(path, &json);
                json.zeroize();
                res?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn from_file_bytes(bytes: &[u8]) -> Result<Self> {
        let f: IdentityFile = serde_json::from_slice(bytes)?;
        let mut raw = b32_decode(&f.secret).ok_or(Error::BadIdentity("clé privée"))?;
        let secret: Option<[u8; 32]> = raw.as_slice().try_into().ok();
        raw.zeroize();
        let secret = secret.ok_or(Error::BadIdentity("clé privée"))?;
        let public: [u8; 32] = b32_decode(&f.public)
            .and_then(|v| v.try_into().ok())
            .ok_or(Error::BadIdentity("clé publique"))?;
        // La clé publique est TOUJOURS recalculée depuis la clé privée : un fichier modifié
        // (public d'un autre) ne peut pas faire annoncer une identité qui n'est pas la nôtre.
        if public_from_secret(&secret) != public {
            return Err(Error::BadIdentity("clé publique incohérente avec la clé privée"));
        }
        Ok(Self { secret, public })
    }

    pub fn public(&self) -> PublicKey {
        self.public
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }

    pub(crate) fn secret(&self) -> &[u8; 32] {
        &self.secret
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.fingerprint())
    }
}

/// Empreinte lisible à comparer à l'oeil : `ABCD-EFGH-IJKL-MNOP-QRST-UVWX`.
pub fn fingerprint(pk: &PublicKey) -> String {
    let digest = Sha256::digest(pk);
    let s = b32_encode(&digest[..15]).to_uppercase();
    s.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

/// Clé publique X25519 correspondant à une clé privée (même dérivation que Noise/snow).
fn public_from_secret(secret: &[u8; 32]) -> PublicKey {
    curve25519_dalek::MontgomeryPoint::mul_base_clamped(*secret).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_is_derived_from_secret() {
        let id = Identity::generate();
        assert_eq!(public_from_secret(&id.secret), id.public);
    }

    #[test]
    fn inconsistent_identity_file_is_refused() {
        let a = Identity::generate();
        let b = Identity::generate();
        let good = IdentityFile { secret: b32_encode(&a.secret), public: b32_encode(&a.public) };
        assert!(Identity::from_file_bytes(&serde_json::to_vec(&good).unwrap()).is_ok());
        // clé publique d'une autre identité : refusé
        let bad = IdentityFile { secret: b32_encode(&a.secret), public: b32_encode(&b.public) };
        assert!(matches!(
            Identity::from_file_bytes(&serde_json::to_vec(&bad).unwrap()),
            Err(Error::BadIdentity(_))
        ));
    }
}
